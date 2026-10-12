use anyhow::{Context, anyhow, bail};
use clap::{
    Parser, Subcommand,
    builder::{PossibleValuesParser, TypedValueParser},
};
use seirei::{Started, config::Config};
use seirei_proto::{
    home,
    ids::MachineId,
    model::{self, Access, CoordinatorView, MachineProjects, MachineStatus, PAIRING_HINT, ProjectView},
    timeline::Timeline,
    wire::{self, BuildId, CallId, ClientMsg, Command, Reply, ServerMsg, Subscription},
};
use serde::Serialize;
use std::{
    io::{self, IsTerminal},
    os::unix::process::CommandExt,
    process::Stdio,
    sync::Arc,
    time::Duration,
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
    /// Open this project directly: `[machine/]project`, the project by title or id.
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
    /// Pair with a machine's T3 and save a 30-day credential. Needed once per machine, and again
    /// when it lapses.
    Login {
        /// The machine to pair with.
        #[arg(default_value = MachineId::LOCAL, value_parser = MachineId::parse)]
        machine: MachineId,
        /// The most autonomy seirei may act with. Threads above it are read-only to seirei.
        #[arg(long, default_value = "full-access", value_parser = access())]
        access: Access,
        /// A pairing code from T3 on that machine. Without one, seirei runs `t3 auth pairing
        /// create` for `local`, and asks for a code for any other machine.
        #[arg(long)]
        code: Option<String>,
    },
    /// Add, list, or remove the T3 servers seirei reaches on other machines.
    Machine {
        #[command(subcommand)]
        command: MachineCmd,
    },
    /// Print what the daemon knows, as JSON: every machine and its projects, or one project's pane.
    Status {
        /// `[machine/]project`, the project by title or id.
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum MachineCmd {
    /// Reach the T3 at `address` under `name`. For now it must be a Tailscale address, like
    /// https://macbook.tail1234.ts.net:4043.
    Add {
        #[arg(value_parser = MachineId::parse)]
        name: MachineId,
        address: String,
    },
    /// Every machine and its address, with its status when a daemon is running.
    List,
    /// Stop reaching a machine. Its credential and records stay on disk.
    Remove {
        #[arg(value_parser = MachineId::parse)]
        name: MachineId,
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
        Some(Cmd::Login { machine, access, code }) => runtime.block_on(login(machine, access, code)),
        Some(Cmd::Machine { command: MachineCmd::Add { name, address } }) => {
            runtime.block_on(add_machine(name, address))
        }
        Some(Cmd::Machine { command: MachineCmd::List }) => runtime.block_on(list_machines()),
        Some(Cmd::Machine { command: MachineCmd::Remove { name } }) => runtime.block_on(remove_machine(name)),
        Some(Cmd::Status { project }) => runtime.block_on(status(project)),
        None => runtime.block_on(seirei_tui::run(seirei_tui::Launch {
            socket: home::socket()?,
            open: cli.project,
            spawn_daemon,
        })),
    }
}

/// Starts `seirei daemon` detached: its own process group, stdio to the daemon log, so closing
/// the terminal does not take it down.
fn spawn_daemon() -> io::Result<()> {
    let state_dir = Config::load().map_err(io::Error::other)?.state_dir;
    seirei::create_private_dir(&state_dir)?;
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

/// The daemon pairs and saves the credential. Only the prompt for a remote's code runs here.
async fn login(machine: MachineId, access: Access, code: Option<String>) -> anyhow::Result<()> {
    Config::load()?.machine(&machine)?;
    let code = match code {
        None if !machine.is_local() => Some(ask_for_code(&machine)?),
        code => code,
    };
    let mut client = Client::ready().await?;
    client.call(Command::Login { machine: machine.clone(), access, code }).await?;
    println!("Logged in to {machine} with {}.", access.name());
    report(&mut client, &machine).await
}

fn ask_for_code(machine: &MachineId) -> anyhow::Result<String> {
    if !io::stdin().is_terminal() {
        bail!("{PAIRING_HINT}, then run `seirei login {machine} --code <code>`.");
    }
    eprint!("{PAIRING_HINT}.\nPairing code for {machine}: ");
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    match line.trim() {
        "" => bail!("no pairing code given"),
        code => Ok(code.into()),
    }
}

async fn add_machine(name: MachineId, address: String) -> anyhow::Result<()> {
    let mut client = Client::ready().await?;
    client.call(Command::AddMachine { name: name.clone(), address }).await?;
    println!("Added {name}.");
    report(&mut client, &name).await
}

async fn remove_machine(name: MachineId) -> anyhow::Result<()> {
    Client::ready().await?.call(Command::RemoveMachine { machine: name.clone() }).await?;
    println!("Removed {name}.");
    Ok(())
}

async fn report(client: &mut Client, machine: &MachineId) -> anyhow::Result<()> {
    let Reply::Projects(listed) = client.call(Command::ListProjects).await? else {
        bail!("ListProjects answered without projects")
    };
    if let Some(listed) = listed.iter().find(|listed| &listed.machine.id == machine) {
        println!("{machine}: {}", reach(listed));
    }
    Ok(())
}

fn reach(MachineProjects { machine, projects }: &MachineProjects) -> String {
    match (&machine.status, &machine.label) {
        (MachineStatus::Connected, Some(label)) => format!("connected to {label}, {} projects", projects.len()),
        _ => machine.status_text(),
    }
}

async fn list_machines() -> anyhow::Result<()> {
    let config = Config::load()?;
    let listed = running_listing().await?;
    let status = |id: &MachineId| match listed.as_ref().map(|listed| listed.iter().find(|l| &l.machine.id == id)) {
        None => String::new(),
        Some(None) => "not known to the running daemon".into(),
        Some(Some(listed)) => reach(listed),
    };
    let rows: Vec<[String; 3]> = (config.machines.iter())
        .map(|machine| [machine.id.to_string(), machine.endpoint.address(), status(&machine.id)])
        .collect();
    let width = |column: usize| rows.iter().map(|row| row[column].len()).max().unwrap_or(0);
    let (names, addresses) = (width(0), width(1));
    for [id, address, status] in &rows {
        println!("{}", format!("{id:names$}  {address:addresses$}  {status}").trim_end());
    }
    if listed.is_none() {
        println!("No daemon is running, so no status. `seirei status` starts one.");
    }
    Ok(())
}

/// The running daemon's view of every machine, without starting a daemon.
async fn running_listing() -> anyhow::Result<Option<Vec<MachineProjects>>> {
    let Some(mut client) = Client::connect().await? else { return Ok(None) };
    if client.hello(false).await? != ServerMsg::Welcome {
        return Ok(None);
    }
    match client.call(Command::ListProjects).await? {
        Reply::Projects(listed) => Ok(Some(listed)),
        Reply::Done => bail!("ListProjects answered without projects"),
    }
}

async fn status(project: Option<String>) -> anyhow::Result<()> {
    let mut client = Client::ready().await?;
    let Reply::Projects(projects) = client.call(Command::ListProjects).await? else {
        bail!("ListProjects answered without projects")
    };
    let Some(wanted) = project else {
        println!("{}", serde_json::to_string_pretty(&projects)?);
        return Ok(());
    };
    let project = model::find_project(&projects, &wanted).map_err(|e| anyhow!(e))?;
    client.send(&ClientMsg::Subscribe(Subscription { project: Some(project), job: None })).await?;
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
        match UnixStream::connect(home::socket()?).await {
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
        bail!("started a daemon, but it is not listening at {}", home::socket()?.display())
    }

    /// A session with a daemon of this build, started when none runs.
    async fn ready() -> anyhow::Result<Self> {
        let mut client = Self::connect_or_spawn().await?;
        match client.hello(false).await? {
            ServerMsg::Welcome => Ok(client),
            ServerMsg::WrongBuild { daemon } => {
                bail!("the running daemon is another build ({}); stop it with `seirei daemon --stop`", daemon.0)
            }
            other => bail!("unexpected answer to Hello: {other:?}"),
        }
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
    fn login_defaults_to_local_at_full_access() {
        Cli::command().debug_assert();
        let login = |args: &[&str]| match Cli::try_parse_from(args).map(|cli| cli.command) {
            Ok(Some(Cmd::Login { machine, access, .. })) => Some((machine.to_string(), access)),
            _ => None,
        };
        assert_eq!(login(&["seirei", "login", "--access", "auto"]), Some(("local".into(), Access::Auto)));
        assert_eq!(login(&["seirei", "login"]), Some(("local".into(), Access::FullAccess)));
        assert_eq!(login(&["seirei", "login", "macbook"]), Some(("macbook".into(), Access::FullAccess)));
        assert_eq!(login(&["seirei", "login", "--access", "root"]), None);
        assert_eq!(login(&["seirei", "login", "MacBook"]), None, "a machine name is parsed");
    }
}
