use anyhow::{Context, anyhow, bail};
use clap::{
    Parser, Subcommand,
    builder::{PossibleValuesParser, TypedValueParser},
};
use seirei::{
    Started,
    config::Config,
    t3::login::{self, PairingCode},
};
use seirei_proto::{
    model::{Access, CoordinatorView, Project, ProjectView},
    timeline::Timeline,
    wire::{self, BuildId, CallId, ClientMsg, Command, Reply, ServerMsg, Subscription},
};
use serde::Serialize;
use std::{
    io,
    os::unix::process::CommandExt,
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

/// A single pane of glass per project over T3 Code.
#[derive(Parser)]
#[command(name = "seirei", version)]
struct Cli {
    /// Open this project directly, by title or id.
    project: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground. `seirei` starts one in the background when needed.
    Daemon {
        /// Ask the running daemon to exit.
        #[arg(long)]
        stop: bool,
    },
    /// Pair with T3 and save a 30-day credential. Needed once, and again when it lapses.
    Login {
        /// The most autonomy seirei may act with. Threads above it are read-only to seirei.
        #[arg(long, default_value = "full-access", value_parser = access())]
        access: Access,
        /// A pairing code from T3's UI. Without one, seirei runs `t3 auth pairing create`.
        #[arg(long)]
        code: Option<String>,
    },
    /// Print what the daemon knows, as JSON: the project list, or one project's pane.
    Status {
        /// A project title or id.
        project: Option<String>,
    },
}

fn access() -> impl TypedValueParser<Value = Access> {
    PossibleValuesParser::new(Access::ALL.map(Access::name))
        .map(|name| Access::ALL.into_iter().find(|access| access.name() == name).expect("clap admits only these names"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new()?;
    match cli.command {
        Some(Cmd::Daemon { stop: false }) => match runtime.block_on(seirei::run_daemon(Config::load()?))? {
            Started::Served => Ok(()),
            Started::AlreadyRunning => bail!("a daemon is already running; stop it with `seirei daemon --stop`"),
        },
        Some(Cmd::Daemon { stop: true }) => runtime.block_on(stop_daemon()),
        Some(Cmd::Login { access, code }) => runtime.block_on(login(access, code)),
        Some(Cmd::Status { project }) => runtime.block_on(status(project)),
        None => runtime.block_on(seirei_tui::run(seirei_tui::Launch {
            socket: wire::socket_path(),
            open: cli.project,
            spawn_daemon,
        })),
    }
}

/// Starts `seirei daemon` detached: its own process group, stdio to the daemon log, so closing
/// the terminal does not take it down.
fn spawn_daemon() -> io::Result<()> {
    let state_dir = Config::load().map_err(io::Error::other)?.state_dir;
    std::fs::create_dir_all(&state_dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(state_dir.join("daemon.log"))?;
    (std::process::Command::new(std::env::current_exe()?))
        .arg("daemon")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .map(drop)
}

async fn stop_daemon() -> anyhow::Result<()> {
    if stop_running().await? {
        println!("Stopped the daemon.");
    } else {
        println!("No daemon is running.");
    }
    Ok(())
}

async fn stop_running() -> anyhow::Result<bool> {
    let Some(mut client) = Client::connect().await? else { return Ok(false) };
    if client.hello(true).await? == ServerMsg::Welcome {
        client.send(&ClientMsg::Call { id: CallId(0), command: Command::Shutdown }).await?;
    }
    while let Ok(Some(_)) = client.recv().await {}
    Ok(true)
}

async fn login(access: Access, code: Option<String>) -> anyhow::Result<()> {
    let config = Config::load()?;
    let origin = seirei::t3::origin(&config.t3_home)?;
    let credentials = login::login(&origin, access, code.map_or(PairingCode::Mint, PairingCode::Given)).await?;
    login::save(&config.credentials, &credentials)?;
    let days = credentials.expires_at.duration_since(SystemTime::now()).unwrap_or_default().as_secs() / 86_400;
    println!(
        "Saved a {} credential for {origin} to {}. It expires in {days} days.",
        access.name(),
        config.credentials.display()
    );
    if stop_running().await? {
        println!("Stopped the running daemon; the next `seirei` starts one with the new credential.");
    }
    Ok(())
}

async fn status(project: Option<String>) -> anyhow::Result<()> {
    let mut client = Client::connect_or_spawn().await?;
    match client.hello(false).await? {
        ServerMsg::Welcome => {}
        ServerMsg::WrongBuild { daemon } => {
            bail!("the running daemon is another build ({}); stop it with `seirei daemon --stop`", daemon.0)
        }
        other => bail!("unexpected answer to Hello: {other:?}"),
    }
    let Reply::Projects(projects) = client.call(Command::ListProjects).await? else {
        bail!("ListProjects answered without projects")
    };
    let Some(wanted) = project else {
        println!("{}", serde_json::to_string_pretty(&projects)?);
        return Ok(());
    };
    let project = find_project(&projects, &wanted)?;
    client.send(&ClientMsg::Subscribe(Subscription { project: Some(project.id.clone()), job: None })).await?;
    let mut pane = Pane::default();
    let filled = tokio::time::timeout(Duration::from_secs(10), pane.fill(&mut client)).await;
    if pane.view.is_some() {
        println!("{}", serde_json::to_string_pretty(&pane)?);
    }
    match filled {
        Ok(read) => read,
        Err(_) if pane.view.is_some() => bail!("the coordinator's timeline did not arrive within 10 s"),
        Err(_) => bail!("no view of {wanted} within 10 s; the daemon log says why"),
    }
}

fn find_project<'a>(projects: &'a [Project], wanted: &str) -> anyhow::Result<&'a Project> {
    (projects.iter().find(|project| project.id.as_str() == wanted || project.title == wanted))
        .with_context(|| format!("T3 has no project titled {wanted:?} or with that id"))
}

#[derive(Serialize, Default)]
struct Pane {
    view: Option<Arc<ProjectView>>,
    timeline: Option<Arc<Timeline>>,
}

impl Pane {
    async fn fill(&mut self, client: &mut Client) -> anyhow::Result<()> {
        while !self.complete() {
            match client.recv().await?.context("the daemon closed the connection")? {
                ServerMsg::Project(view) => self.view = Some(view),
                ServerMsg::Timeline(timeline) => self.timeline = Some(timeline),
                _ => {}
            }
        }
        Ok(())
    }

    fn complete(&self) -> bool {
        match self.view.as_deref().map(|view| &view.coordinator) {
            None => false,
            Some(CoordinatorView::Ready(coordinator)) => {
                self.timeline.as_ref().is_some_and(|t| t.thread == coordinator.id)
            }
            Some(CoordinatorView::Absent | CoordinatorView::Starting) => true,
        }
    }
}

struct Client {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
    calls: u64,
}

impl Client {
    async fn connect() -> io::Result<Option<Self>> {
        match UnixStream::connect(wire::socket_path()).await {
            Ok(stream) => {
                let (read, write) = stream.into_split();
                Ok(Some(Self { lines: BufReader::new(read).lines(), write, calls: 0 }))
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn connect_or_spawn() -> anyhow::Result<Self> {
        if let Some(client) = Self::connect().await? {
            return Ok(client);
        }
        spawn_daemon()?;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(client) = Self::connect().await? {
                return Ok(client);
            }
        }
        bail!("started a daemon, but it is not listening at {}", wire::socket_path().display())
    }

    async fn hello(&mut self, take_over: bool) -> anyhow::Result<ServerMsg> {
        self.send(&ClientMsg::Hello { build: BuildId::current(), take_over }).await?;
        self.recv().await?.context("the daemon closed the connection during the handshake")
    }

    async fn call(&mut self, command: Command) -> anyhow::Result<Reply> {
        self.calls += 1;
        let id = CallId(self.calls);
        self.send(&ClientMsg::Call { id, command }).await?;
        loop {
            if let ServerMsg::Reply { id: replied, result } =
                self.recv().await?.context("the daemon hung up mid-call")?
                && replied == id
            {
                return result.map_err(|e| anyhow!(e.message));
            }
        }
    }

    async fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        self.write.write_all(&wire::encode(msg)).await
    }

    async fn recv(&mut self) -> anyhow::Result<Option<ServerMsg>> {
        let Some(line) = self.lines.next_line().await? else { return Ok(None) };
        Ok(Some(wire::decode(line.as_bytes())?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn login_takes_access_by_name_and_defaults_to_full_access() {
        Cli::command().debug_assert();
        let access = |args: &[&str]| match Cli::try_parse_from(args).map(|cli| cli.command) {
            Ok(Some(Cmd::Login { access, .. })) => Some(access),
            _ => None,
        };
        assert_eq!(access(&["seirei", "login", "--access", "auto"]), Some(Access::Auto));
        assert_eq!(access(&["seirei", "login"]), Some(Access::FullAccess));
        assert_eq!(access(&["seirei", "login", "--access", "root"]), None);
    }
}
