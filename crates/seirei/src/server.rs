use crate::config::{self, Secret, TailnetOrigin, Token};
use crate::coordinator::ProjectRecord;
use crate::project::{ProjectCommand, ProjectHandle};
use crate::t3::{
    Environment, T3Api, T3Error,
    login::{self, PairingCode},
};
use anyhow::{bail, ensure};
use seirei_proto::{
    ids::{MachineId, ProjectRef, ThreadId},
    model::{
        Access, CoordinatorView, Machine, MachineProjects, MachineStatus, ModelSelection, PAIRING_HINT, Project,
        ProjectView,
    },
    timeline::Timeline,
    wire::{self, BuildId, CallError, ClientMsg, Command, MAX_LINE, Reply, ServerMsg, Subscription},
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{mpsc, watch},
    task::JoinSet,
};

/// How long `ListProjects` waits for any one machine. A machine that misses it is listed as
/// unreachable; it never holds up the others.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Locks are taken in the order `machines`, `identities`, `projects`.
pub struct Daemon<A> {
    /// While the daemon runs it is the only writer of `machines.toml` and the credential files.
    /// Each write happens under this lock together with the registry change it stands for.
    machines: Mutex<BTreeMap<MachineId, Arc<MachineApi<A>>>>,
    config_dir: PathBuf,
    state_dir: PathBuf,
    build: BuildId,
    identities: Mutex<Identities>,
    projects: Mutex<HashMap<ProjectRef, ProjectHandle>>,
    shutdown: watch::Sender<bool>,
}

/// One machine's T3, and how the daemon acts on it.
pub struct MachineApi<A> {
    api: Arc<A>,
    coordinator_model: Option<ModelSelection>,
}

impl<A> MachineApi<A> {
    pub fn new(api: A, coordinator_model: Option<ModelSelection>) -> Self {
        Self { api: Arc::new(api), coordinator_model }
    }
}

impl<A: T3Api> MachineApi<A> {
    async fn list(&self, timeout: Duration) -> Result<(Environment, Vec<Project>), MachineStatus> {
        let reached = async { Ok::<_, T3Error>((self.api.environment().await?, self.api.projects().await?)) };
        match tokio::time::timeout(timeout, reached).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(e)) => Err(machine_status(&e)),
            Err(_) => Err(MachineStatus::Unreachable { detail: format!("no answer within {timeout:?}") }),
        }
    }
}

fn machine_status(e: &T3Error) -> MachineStatus {
    match e {
        T3Error::Unauthorized => MachineStatus::NeedsLogin,
        T3Error::Unreachable(detail) | T3Error::Lost(detail) => MachineStatus::Unreachable { detail: detail.clone() },
        T3Error::NoSession(cause) => machine_status(cause),
        T3Error::Rejected(_) | T3Error::Malformed(_) => MachineStatus::Failed { detail: e.to_string() },
    }
}

/// What the daemon knows of each machine's T3, from `t3_environment_read`. The known identities
/// live on disk, so a duplicate stays one while the machine it duplicates is down.
#[derive(Default)]
struct Identities {
    known: BTreeMap<MachineId, Environment>,
    /// Machines whose address reached another T3 during this daemon's life, with the detail.
    replaced: BTreeMap<MachineId, String>,
}

impl Identities {
    /// The machine first in `MachineId` order among those that share `machine`'s T3, when that
    /// is not `machine` itself.
    fn owner(&self, machine: &MachineId) -> Option<&MachineId> {
        let mine = &self.known.get(machine)?.environment_id;
        let first = self.known.iter().find(|(_, known)| &known.environment_id == mine).map(|(id, _)| id)?;
        (first != machine).then_some(first)
    }
}

fn names<V>(machines: &BTreeMap<MachineId, V>) -> String {
    machines.keys().map(MachineId::as_str).collect::<Vec<_>>().join(", ")
}

fn machine_dir(state_dir: &Path, machine: &MachineId) -> PathBuf {
    state_dir.join("machines").join(machine.as_str())
}

fn read_identity(path: &Path) -> io::Result<Option<Environment>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
}

impl<A: T3Api> Daemon<A> {
    pub fn new(
        machines: impl IntoIterator<Item = (MachineId, MachineApi<A>)>,
        config_dir: PathBuf,
        state_dir: PathBuf,
    ) -> io::Result<Arc<Self>> {
        let machines: BTreeMap<_, _> = machines.into_iter().map(|(id, machine)| (id, Arc::new(machine))).collect();
        let mut known = BTreeMap::new();
        for id in machines.keys() {
            if let Some(identity) = read_identity(&machine_dir(&state_dir, id).join("identity.json"))? {
                known.insert(id.clone(), identity);
            }
        }
        Ok(Arc::new(Self {
            machines: Mutex::new(machines),
            config_dir,
            state_dir,
            build: BuildId::current(),
            identities: Mutex::new(Identities { known, replaced: BTreeMap::new() }),
            projects: Mutex::default(),
            shutdown: watch::Sender::new(false),
        }))
    }

    pub fn warm_start(&self) -> io::Result<()> {
        let machines: Vec<MachineId> = self.machines.lock().expect("no panics while held").keys().cloned().collect();
        let duplicate =
            |machine: &MachineId| self.identities.lock().expect("no panics while held").owner(machine).is_some();
        for machine in machines.iter().filter(|machine| !duplicate(machine)) {
            for project in ProjectRecord::saved(&self.records(machine))? {
                let at = ProjectRef { machine: machine.clone(), project };
                if let Err(e) = self.project(&at) {
                    eprintln!("seirei: not syncing project {at}: {e}");
                }
            }
        }
        Ok(())
    }

    fn records(&self, machine: &MachineId) -> PathBuf {
        machine_dir(&self.state_dir, machine)
    }

    /// A duplicate gets no actor, so one T3 never gets a second coordinator under another name.
    /// The machine stays locked until the actor is registered, so a removal cannot miss it.
    pub fn project(&self, at: &ProjectRef) -> io::Result<ProjectHandle> {
        let machines = self.machines.lock().expect("no panics while held");
        let machine = (machines.get(&at.machine))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no machine is named {}", at.machine)))?;
        let identities = self.identities.lock().expect("no panics while held");
        if let Some(owner) = identities.owner(&at.machine) {
            let refusal = format!("{} reaches the same T3 as {owner}; open its projects through {owner}", at.machine);
            return Err(io::Error::other(refusal));
        }
        let mut projects = self.projects.lock().expect("no panics while held");
        if let Some(handle) = projects.get(at) {
            return Ok(handle.clone());
        }
        let handle = ProjectHandle::spawn(
            machine.api.clone(),
            at.clone(),
            self.records(&at.machine),
            machine.coordinator_model.clone(),
        )?;
        projects.insert(at.clone(), handle.clone());
        Ok(handle)
    }

    /// Every machine is asked at once. Duplicates are decided from the known identities after
    /// all answers are in, so the order the answers arrive in never matters.
    async fn list_projects(&self, timeout: Duration) -> Vec<MachineProjects> {
        let mut listings = JoinSet::new();
        for (id, machine) in self.machines.lock().expect("no panics while held").iter() {
            let (id, machine) = (id.clone(), machine.clone());
            listings.spawn(async move { (id, machine.api.address(), machine.list(timeout).await) });
        }
        let mut answers = listings.join_all().await;
        answers.sort_by(|(a, ..), (b, ..)| a.cmp(b));
        let registered = self.machines.lock().expect("no panics while held");
        let answers: Vec<_> = (answers.into_iter())
            .filter(|(id, ..)| registered.contains_key(id))
            .map(|(id, address, answer)| {
                let projects = answer.and_then(|(environment, projects)| {
                    let failed =
                        |e: io::Error| MachineStatus::Failed { detail: format!("recording its identity: {e}") };
                    self.learn(&id, environment).map_err(failed)?;
                    Ok(projects)
                });
                (id, address, projects)
            })
            .collect();
        drop(registered);
        let identities = self.identities.lock().expect("no panics while held");
        (answers.into_iter())
            .map(|(id, address, answer)| {
                let label = identities.known.get(&id).map(|known| known.label.clone());
                let (status, projects) = match (identities.owner(&id), answer) {
                    (Some(owner), _) => (MachineStatus::Duplicate { of: owner.clone() }, Vec::new()),
                    (None, Err(status)) => (status, Vec::new()),
                    (None, Ok(projects)) => match identities.replaced.get(&id) {
                        Some(detail) => (MachineStatus::Replaced { detail: detail.clone() }, projects),
                        None => (MachineStatus::Connected, projects),
                    },
                };
                MachineProjects { machine: Machine { id, address, label, status }, projects }
            })
            .collect()
    }

    /// Records the T3 `machine` reached. When that is another T3 than before, the machine's
    /// project records and actors go: their coordinator bindings name the old server's threads.
    fn learn(&self, machine: &MachineId, read: Environment) -> io::Result<()> {
        let mut identities = self.identities.lock().expect("no panics while held");
        match identities.known.get(machine) {
            Some(known) if *known == read => return Ok(()),
            Some(known) if known.environment_id != read.environment_id => {
                let detail = format!(
                    "this address now reaches another T3, {} ({}), not {} ({}); seirei forgot the coordinators it \
                     knew there",
                    read.label, read.environment_id, known.label, known.environment_id
                );
                self.stop_projects(machine);
                ProjectRecord::forget_all(&self.records(machine))?;
                identities.replaced.insert(machine.clone(), detail);
            }
            Some(_) | None => {}
        }
        let json = serde_json::to_vec_pretty(&read).expect("identities serialize infallibly");
        crate::write_private(&self.records(machine).join("identity.json"), &json)?;
        identities.known.insert(machine.clone(), read);
        Ok(())
    }

    /// Stops every actor of `machine`. A session watching one of them sees its feed close.
    fn stop_projects(&self, machine: &MachineId) {
        let mut projects = self.projects.lock().expect("no panics while held");
        for (_, handle) in projects.extract_if(|at, _| &at.machine == machine) {
            handle.stop();
        }
    }

    fn machine(&self, id: &MachineId) -> anyhow::Result<Arc<MachineApi<A>>> {
        let machines = self.machines.lock().expect("no panics while held");
        match machines.get(id) {
            Some(machine) => Ok(machine.clone()),
            None => bail!("no machine is named {id}; seirei knows {}", names(&machines)),
        }
    }

    /// Converges on `id` reaching `address`, both in `machines.toml` and in the registry.
    fn add_machine(&self, id: &MachineId, address: &str) -> anyhow::Result<()> {
        let origin = TailnetOrigin::parse(address)?;
        let mut machines = self.machines.lock().expect("no panics while held");
        let added = config::add_machine(&self.config_dir, id, &origin)?;
        if machines.contains_key(id) {
            return Ok(());
        }
        if let Some(identity) = read_identity(&self.records(id).join("identity.json"))? {
            self.identities.lock().expect("no panics while held").known.insert(id.clone(), identity);
        }
        let api = A::connect(added.endpoint, added.token);
        machines.insert(id.clone(), Arc::new(MachineApi::new(api, added.coordinator_model)));
        Ok(())
    }

    /// Its credential and state folder stay, so adding it back picks up where it left off.
    fn remove_machine(&self, id: &MachineId) -> anyhow::Result<()> {
        let mut machines = self.machines.lock().expect("no panics while held");
        let listed = config::remove_machine(&self.config_dir, id)?;
        if machines.remove(id).is_none() && !listed {
            bail!("no machine is named {id}; seirei knows {}", names(&machines));
        }
        let mut identities = self.identities.lock().expect("no panics while held");
        identities.known.remove(id);
        identities.replaced.remove(id);
        drop(identities);
        self.stop_projects(id);
        match std::fs::remove_file(config::credentials(&self.config_dir, id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// The pairing runs unlocked, since it waits on T3. The new credential is saved and swapped
    /// into the machine's connection only if that connection is still the registered one.
    async fn login(&self, id: &MachineId, access: Access, code: Option<String>) -> anyhow::Result<()> {
        let machine = self.machine(id)?;
        let code = match code {
            Some(code) => PairingCode::Given(code),
            None if id.is_local() => PairingCode::Mint,
            None => bail!("{id} needs a pairing code. {PAIRING_HINT}."),
        };
        let credentials = machine.api.pair(access, code).await?;
        let machines = self.machines.lock().expect("no panics while held");
        let current = machines.get(id).is_some_and(|held| Arc::ptr_eq(held, &machine));
        ensure!(current, "{id} was removed while seirei paired with it, so the credential was not saved");
        login::save(&config::credentials(&self.config_dir, id), &credentials)?;
        machine.api.use_token(Token { secret: Secret(credentials.token), access: Some(credentials.access) });
        Ok(())
    }

    pub async fn serve(self: Arc<Self>, listener: UnixListener) -> io::Result<()> {
        let mut shutdown = self.shutdown.subscribe();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let daemon = self.clone();
                    let (stream, _) = accepted?;
                    tokio::spawn(async move {
                        if let Err(e) = session(daemon, stream).await {
                            eprintln!("seirei: session ended: {e}");
                        }
                    });
                }
                _ = shutdown.wait_for(|stop| *stop) => return Ok(()),
            }
        }
    }

    async fn call(&self, command: Command) -> Result<Reply, CallError> {
        let refused = |e: anyhow::Error| CallError { message: format!("{e:#}"), retry_safe: true };
        let (project, command) = match command {
            Command::ListProjects => return Ok(Reply::Projects(self.list_projects(LIST_TIMEOUT).await)),
            Command::AddMachine { name, address } => {
                return self.add_machine(&name, &address).map(|()| Reply::Done).map_err(refused);
            }
            Command::RemoveMachine { machine } => {
                return self.remove_machine(&machine).map(|()| Reply::Done).map_err(refused);
            }
            Command::Login { machine, access, code } => {
                return self.login(&machine, access, code).await.map(|()| Reply::Done).map_err(refused);
            }
            Command::Shutdown => {
                self.shutdown.send_replace(true);
                return Ok(Reply::Done);
            }
            Command::EnsureCoordinator { project } => (project, ProjectCommand::EnsureCoordinator),
            Command::Send { project, thread, text, send_id } => {
                (project, ProjectCommand::Send { thread, text, send_id })
            }
            Command::Answer { project, request, answers } => (project, ProjectCommand::Answer { request, answers }),
        };
        let handle = self.project(&project).map_err(|e| CallError { message: e.to_string(), retry_safe: true })?;
        handle.call(command).await
    }
}

pub async fn session<A, S>(daemon: Arc<Daemon<A>>, stream: S) -> io::Result<()>
where
    A: T3Api,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read, write) = tokio::io::split(stream);
    let (lines_tx, lines) = mpsc::channel(16);
    let reader = tokio::spawn(read_lines(read, lines_tx));
    let result = converse(&daemon, lines, write).await;
    reader.abort();
    result
}

type Lines = mpsc::Receiver<io::Result<Vec<u8>>>;

async fn read_lines<R: AsyncRead + Unpin>(read: R, lines: mpsc::Sender<io::Result<Vec<u8>>>) {
    let mut reader = BufReader::new(read);
    loop {
        let mut buf = Vec::new();
        let line = match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await {
            Ok(_) if buf.ends_with(b"\n") => {
                buf.pop();
                Ok(buf)
            }
            Ok(_) if buf.len() > MAX_LINE => {
                Err(io::Error::new(io::ErrorKind::InvalidData, "line longer than MAX_LINE"))
            }
            Ok(_) => return,
            Err(e) => Err(e),
        };
        let failed = line.is_err();
        if lines.send(line).await.is_err() || failed {
            return;
        }
    }
}

async fn converse<A: T3Api, W: AsyncWrite + Unpin>(
    daemon: &Arc<Daemon<A>>,
    mut lines: Lines,
    mut out: W,
) -> io::Result<()> {
    let Some(first) = lines.recv().await.transpose()? else { return Ok(()) };
    let Ok(ClientMsg::Hello { build, take_over }) = wire::decode(&first) else { return Ok(()) };
    let (answer, next) = greet(&daemon.build, &build, take_over);
    out.write_all(&wire::encode(&answer)).await?;
    out.flush().await?;
    match next {
        Continue::Session => {}
        Continue::Close => return Ok(()),
        Continue::ShutDown => {
            daemon.shutdown.send_replace(true);
            return Ok(());
        }
    }
    let (replies_tx, mut replies) = mpsc::unbounded_channel();
    let mut feeds = Feeds::default();
    loop {
        let frames = tokio::select! {
            line = lines.recv() => {
                let Some(line) = line.transpose()? else { return Ok(()) };
                match wire::decode::<ClientMsg>(&line) {
                    Ok(ClientMsg::Subscribe(wanted)) => feeds.subscribe(daemon, wanted).await,
                    Ok(ClientMsg::Call { id, command }) => {
                        let (daemon, replies) = (daemon.clone(), replies_tx.clone());
                        tokio::spawn(async move {
                            let result = daemon.call(command).await;
                            let _ = replies.send(ServerMsg::Reply { id, result });
                        });
                        Vec::new()
                    }
                    Ok(ClientMsg::Hello { .. }) => return Err(protocol("a second Hello")),
                    Err(e) => return Err(protocol(e)),
                }
            }
            Some(reply) = replies.recv() => vec![reply],
            frames = feeds.changed() => frames,
        };
        for frame in frames {
            out.write_all(&wire::encode(&frame)).await?;
        }
        out.flush().await?;
    }
}

fn protocol(detail: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("client broke the protocol: {detail}"))
}

fn greet(daemon: &BuildId, client: &BuildId, take_over: bool) -> (ServerMsg, Continue) {
    match (client == daemon, take_over) {
        (true, _) => (ServerMsg::Welcome, Continue::Session),
        (false, true) => (ServerMsg::Yielding, Continue::ShutDown),
        (false, false) => (ServerMsg::WrongBuild { daemon: daemon.clone() }, Continue::Close),
    }
}

enum Continue {
    Session,
    Close,
    ShutDown,
}

#[derive(Default)]
struct Feeds {
    subscription: Subscription,
    project: Option<(ProjectHandle, watch::Receiver<Option<Arc<ProjectView>>>)>,
    coordinator: TimelineFeed,
    job: TimelineFeed,
}

type TimelineFeed = Option<(ThreadId, watch::Receiver<Arc<Timeline>>)>;

enum Change {
    View(Arc<ProjectView>),
    Timeline(Arc<Timeline>),
    Stopped,
}

impl Feeds {
    async fn subscribe<A: T3Api>(&mut self, daemon: &Daemon<A>, wanted: Subscription) -> Vec<ServerMsg> {
        if wanted.project != self.subscription.project {
            self.project = wanted.project.as_ref().and_then(|id| match daemon.project(id) {
                Ok(handle) => Some((handle.clone(), handle.view())),
                Err(e) => {
                    eprintln!("seirei: cannot sync project {id}: {e}");
                    None
                }
            });
            (self.coordinator, self.job) = (None, None);
        }
        self.subscription = wanted;
        let mut frames = Vec::new();
        if let Some((_, view)) = &mut self.project
            && let Some(view) = view.borrow_and_update().clone()
        {
            frames.push(ServerMsg::Project(view));
        }
        self.follow(&mut frames).await;
        frames
    }

    async fn changed(&mut self) -> Vec<ServerMsg> {
        let change = tokio::select! {
            view = next_view(&mut self.project) => view.map_or(Change::Stopped, Change::View),
            timeline = next_timeline(&mut self.coordinator) => Change::Timeline(timeline),
            timeline = next_timeline(&mut self.job) => Change::Timeline(timeline),
        };
        match change {
            Change::Timeline(timeline) => vec![ServerMsg::Timeline(timeline)],
            Change::Stopped => {
                *self = Self::default();
                Vec::new()
            }
            Change::View(view) => {
                let mut frames = vec![ServerMsg::Project(view)];
                self.follow(&mut frames).await;
                frames
            }
        }
    }

    async fn follow(&mut self, frames: &mut Vec<ServerMsg>) {
        let Some((handle, view)) = &self.project else { return };
        let coordinator = match view.borrow().as_deref().map(|view| &view.coordinator) {
            Some(CoordinatorView::Ready(thread)) => Some(thread.id.clone()),
            _ => None,
        };
        reopen(handle, &mut self.coordinator, coordinator, frames).await;
        reopen(handle, &mut self.job, self.subscription.job.clone(), frames).await;
    }
}

async fn reopen(
    handle: &ProjectHandle,
    feed: &mut TimelineFeed,
    wanted: Option<ThreadId>,
    frames: &mut Vec<ServerMsg>,
) {
    if feed.as_ref().map(|(id, _)| id) == wanted.as_ref() {
        return;
    }
    *feed = match wanted {
        Some(id) => handle.open_timeline(id.clone()).await.map(|timeline| (id, timeline)),
        None => None,
    };
    if let Some((_, timeline)) = feed {
        let current = timeline.borrow_and_update().clone();
        if current.detail.is_some() {
            frames.push(ServerMsg::Timeline(current));
        }
    }
}

/// The next view, or `None` once the project's actor has stopped.
async fn next_view(
    feed: &mut Option<(ProjectHandle, watch::Receiver<Option<Arc<ProjectView>>>)>,
) -> Option<Arc<ProjectView>> {
    let Some((handle, view)) = feed else { return std::future::pending().await };
    let changed = async {
        while view.changed().await.is_ok() {
            if let Some(current) = view.borrow_and_update().clone() {
                return current;
            }
        }
        std::future::pending().await
    };
    tokio::select! {
        current = changed => Some(current),
        () = handle.stopped() => None,
    }
}

async fn next_timeline(feed: &mut TimelineFeed) -> Arc<Timeline> {
    if let Some((_, timeline)) = feed
        && timeline.changed().await.is_ok()
    {
        return timeline.borrow_and_update().clone();
    }
    std::future::pending().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::{Binding, CoordinatorKey};
    use crate::t3::fake::{Down, FakeT3};
    use seirei_proto::{
        ids::{ProjectId, SendId, T3Time},
        model::{Lineage, Problem, Project},
        wire::{CallId, Command},
    };
    use std::path::Path;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    fn seirei_code() -> Project {
        Project {
            id: ProjectId::new("480923ae-52f3-431e-a558-42c44e1b9fb3"),
            title: "seirei-code".into(),
            root: "/home/gquental/.t3/projects/seirei-code".into(),
            default_model: None,
        }
    }

    struct Client {
        lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
        write: WriteHalf<DuplexStream>,
    }

    impl Client {
        fn connect(daemon: &Arc<Daemon<FakeT3>>) -> Self {
            let (client, server) = tokio::io::duplex(1 << 16);
            tokio::spawn(session(daemon.clone(), server));
            let (read, write) = tokio::io::split(client);
            Self { lines: BufReader::new(read).lines(), write }
        }

        async fn send(&mut self, msg: ClientMsg) {
            self.write.write_all(&wire::encode(&msg)).await.expect("the session is reading");
        }

        async fn recv(&mut self) -> Option<ServerMsg> {
            let next = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line());
            let line = next.await.expect("the daemon answers within 5 s").expect("the stream is readable")?;
            Some(wire::decode(line.as_bytes()).expect("every frame decodes"))
        }
    }

    fn machine(name: &str) -> MachineId {
        MachineId::parse(name).expect("a machine name")
    }

    fn daemon(dir: &Path, machines: impl IntoIterator<Item = (&'static str, FakeT3)>) -> Arc<Daemon<FakeT3>> {
        let machines = machines.into_iter().map(|(name, api)| (machine(name), MachineApi::new(api, None)));
        Daemon::new(machines, dir.join("config"), dir.to_path_buf()).expect("the identities load")
    }

    fn fake(daemon: &Daemon<FakeT3>, name: &str) -> Arc<FakeT3> {
        daemon.machines.lock().unwrap()[&machine(name)].api.clone()
    }

    async fn reply(client: &mut Client, call: CallId) -> Result<Reply, CallError> {
        loop {
            match client.recv().await {
                Some(ServerMsg::Reply { id, result }) if id == call => return result,
                Some(ServerMsg::Project(_) | ServerMsg::Timeline(_)) => continue,
                other => panic!("expected the reply to {call:?}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_session_routes_each_project_to_its_own_machine() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon =
            daemon(dir.path(), [("local", FakeT3::new(seirei_code())), ("studio", FakeT3::new(seirei_code()))]);
        let mut client = Client::connect(&daemon);
        client.send(ClientMsg::Hello { build: BuildId::current(), take_over: false }).await;
        assert_eq!(client.recv().await, Some(ServerMsg::Welcome));

        let studio = ProjectRef { machine: machine("studio"), project: seirei_code().id };
        client.send(ClientMsg::Subscribe(Subscription { project: Some(studio.clone()), job: None })).await;
        match client.recv().await {
            Some(ServerMsg::Project(view)) => {
                assert_eq!((&view.machine, &view.project), (&studio.machine, &seirei_code()));
                assert_eq!(view.coordinator, CoordinatorView::Absent);
            }
            other => panic!("the first frame after Subscribe is the project view, got {other:?}"),
        }

        let send_id = SendId::fresh();
        let send = Command::Send {
            project: studio.clone(),
            thread: ThreadId::new("mcp:coordinator"),
            text: "Plan the cache".into(),
            send_id: send_id.clone(),
        };
        client.send(ClientMsg::Call { id: CallId(7), command: send }).await;
        assert_eq!(reply(&mut client, CallId(7)).await, Ok(Reply::Done), "the reply answers the call");
        let sent = fake(&daemon, "studio").with(|t3| t3.sends.clone());
        assert_eq!(sent, [(ThreadId::new("mcp:coordinator"), "Plan the cache".to_owned(), send_id)]);
        assert!(fake(&daemon, "local").with(|t3| t3.sends.is_empty()), "the same project id on local is not studio's");

        client.send(ClientMsg::Call { id: CallId(8), command: Command::EnsureCoordinator { project: studio } }).await;
        assert_eq!(reply(&mut client, CallId(8)).await, Ok(Reply::Done));
        assert_eq!(fake(&daemon, "studio").with(|t3| t3.launched.len()), 1);
        assert_eq!(fake(&daemon, "local").with(|t3| t3.launched.len()), 0);
        let record = format!("machines/studio/projects/{}.json", seirei_code().id.as_str());
        assert!(dir.path().join(record).exists(), "each machine keeps its own records");
    }

    #[tokio::test]
    async fn listing_projects_reports_every_machine_and_never_fails_for_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let down = |down| {
            let api = FakeT3::new(seirei_code());
            api.with(|t3| t3.down = Some(down));
            api
        };
        let daemon = daemon(
            dir.path(),
            [
                ("studio", FakeT3::new(seirei_code())),
                ("macbook", down(Down::LoggedOut)),
                ("quiet", down(Down::Silent)),
                ("local", down(Down::Refused)),
            ],
        );
        let listed = daemon.list_projects(Duration::from_millis(200)).await;
        let shown: Vec<(&str, &MachineStatus, Vec<&str>)> = (listed.iter())
            .map(|listed| {
                let titles = listed.projects.iter().map(|project| project.title.as_str()).collect();
                (listed.machine.id.as_str(), &listed.machine.status, titles)
            })
            .collect();
        let unreachable = |detail: &str| MachineStatus::Unreachable { detail: detail.into() };
        assert_eq!(
            shown,
            [
                ("local", &unreachable("connection refused"), vec![]),
                ("macbook", &MachineStatus::NeedsLogin, vec![]),
                ("quiet", &unreachable("no answer within 200ms"), vec![]),
                ("studio", &MachineStatus::Connected, vec!["seirei-code"]),
            ],
            "local first, then by name; a failing machine lists nothing and fails nothing"
        );
    }

    fn twin(environment: &Environment) -> FakeT3 {
        let api = FakeT3::new(seirei_code());
        api.with(|t3| t3.environment = environment.clone());
        api
    }

    fn mac_studio() -> Environment {
        Environment { environment_id: "ad5e9cf0".into(), label: "mac-studio".into() }
    }

    async fn statuses(daemon: &Daemon<FakeT3>) -> Vec<(String, MachineStatus, usize, Option<String>)> {
        (daemon.list_projects(LIST_TIMEOUT).await.into_iter())
            .map(|l| (l.machine.id.to_string(), l.machine.status, l.projects.len(), l.machine.label))
            .collect()
    }

    #[tokio::test]
    async fn a_second_address_for_one_t3_stays_a_duplicate_while_the_first_is_down() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(
            dir.path(),
            [
                ("studio", twin(&mac_studio())),
                ("local", twin(&mac_studio())),
                ("macbook", FakeT3::new(seirei_code())),
                ("alpha", twin(&mac_studio())),
            ],
        );
        fake(&daemon, "alpha").with(|t3| t3.down = Some(Down::Refused));
        let duplicate = MachineStatus::Duplicate { of: MachineId::local() };
        let studio = Some("mac-studio".to_owned());
        let refused = MachineStatus::Unreachable { detail: "connection refused".into() };
        let macbook = fake(&daemon, "macbook").with(|t3| t3.environment.label.clone());
        assert_eq!(
            statuses(&daemon).await,
            [
                ("local".into(), MachineStatus::Connected, 1, studio.clone()),
                ("alpha".into(), refused.clone(), 0, None),
                ("macbook".into(), MachineStatus::Connected, 1, Some(macbook)),
                ("studio".into(), duplicate.clone(), 0, studio.clone()),
            ],
            "local comes first, so studio is the duplicate, and alpha's T3 is not known yet"
        );

        fake(&daemon, "local").with(|t3| t3.down = Some(Down::Refused));
        let listed = statuses(&daemon).await;
        assert_eq!(listed[0], ("local".into(), refused, 0, studio.clone()));
        assert_eq!(listed[3], ("studio".into(), duplicate, 0, studio), "studio never stands in for local");
    }

    #[tokio::test]
    async fn a_duplicate_takes_no_commands_and_runs_no_actors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(dir.path(), [("local", twin(&mac_studio())), ("studio", twin(&mac_studio()))]);
        statuses(&daemon).await;
        let studio = ProjectRef { machine: machine("studio"), project: seirei_code().id };
        let refused =
            daemon.call(Command::EnsureCoordinator { project: studio.clone() }).await.expect_err("a duplicate");
        assert!(refused.message.contains("same T3 as local"), "the refusal names the owner: {}", refused.message);
        assert_eq!(fake(&daemon, "studio").with(|t3| t3.launch_calls), 0);
        assert!(daemon.project(&studio).is_err());
        assert!(daemon.projects.lock().unwrap().is_empty(), "no actor was spawned for studio");
    }

    #[tokio::test]
    async fn known_identities_survive_a_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = daemon(dir.path(), [("local", twin(&mac_studio())), ("studio", twin(&mac_studio()))]);
        statuses(&first).await;
        drop(first);
        let studio_records = dir.path().join("machines/studio");
        ProjectRecord { binding: None }.save(&studio_records, &seirei_code().id).expect("a record from before");

        let down = twin(&mac_studio());
        down.with(|t3| t3.down = Some(Down::Refused));
        let restarted = daemon(dir.path(), [("local", down), ("studio", twin(&mac_studio()))]);
        restarted.warm_start().expect("records load");
        assert!(restarted.projects.lock().unwrap().is_empty(), "a known duplicate's records start no actor");
        let listed = statuses(&restarted).await;
        assert_eq!(listed[1].1, MachineStatus::Duplicate { of: MachineId::local() }, "{listed:?}");
    }

    #[tokio::test]
    async fn an_address_that_reaches_another_t3_forgets_the_old_coordinators() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(dir.path(), [("macbook", FakeT3::new(seirei_code()))]);
        statuses(&daemon).await;
        let records = dir.path().join("machines/macbook");
        let binding = Binding { key: CoordinatorKey::mint(), minted_after: T3Time(0), thread: None };
        ProjectRecord { binding: Some(binding) }.save(&records, &seirei_code().id).expect("record saved");
        let at = ProjectRef { machine: machine("macbook"), project: seirei_code().id };
        daemon.project(&at).expect("an actor for the old T3");

        let mini = Environment { environment_id: "env-mini".into(), label: "mini".into() };
        fake(&daemon, "macbook").with(|t3| t3.environment = mini.clone());
        let listed = daemon.list_projects(LIST_TIMEOUT).await;
        match &listed[0].machine.status {
            MachineStatus::Replaced { detail } => assert!(detail.contains("another T3, mini"), "{detail}"),
            other => panic!("expected the change to be reported, got {other:?}"),
        }
        assert_eq!(listed[0].projects.len(), 1, "the new T3's projects are listed");
        assert!(!records.join("projects").exists(), "the old server's records are gone");
        assert!(daemon.projects.lock().unwrap().is_empty(), "and so are their actors");
        let saved = read_identity(&records.join("identity.json")).expect("readable");
        assert_eq!(saved, Some(mini), "the new identity replaced the old one");
    }

    #[tokio::test]
    async fn a_job_the_actor_does_not_know_never_spins_the_sync_loop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let api = FakeT3::new(seirei_code());
        let coordinator = api.with(|t3| t3.add("mcp:coordinator", "Coordinator", Lineage::Root { via_mcp: true }));
        let binding = Binding { key: CoordinatorKey::mint(), minted_after: T3Time(0), thread: Some(coordinator) };
        let records = dir.path().join("machines/local");
        ProjectRecord { binding: Some(binding) }.save(&records, &seirei_code().id).expect("record saved");
        let daemon = daemon(dir.path(), [("local", api)]);
        let mut client = Client::connect(&daemon);
        client.send(ClientMsg::Hello { build: BuildId::current(), take_over: false }).await;
        assert_eq!(client.recv().await, Some(ServerMsg::Welcome));

        let project = ProjectRef { machine: MachineId::local(), project: seirei_code().id };
        let wanted = Subscription { project: Some(project), job: Some(ThreadId::new("mcp:stranger")) };
        client.send(ClientMsg::Subscribe(wanted)).await;
        assert!(matches!(client.recv().await, Some(ServerMsg::Project(_))), "the view still streams");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let scans = fake(&daemon, "local").with(|t3| t3.list_calls);
        assert!(scans <= 2, "retrying the unknown job must not force ticks: {scans} scans in 0.5 s");
    }

    const STUDIO: &str = "https://mac-studio.tail123b88.ts.net";

    fn add(name: &str, address: &str) -> Command {
        Command::AddMachine { name: machine(name), address: address.into() }
    }

    async fn settled(
        view: &mut watch::Receiver<Option<Arc<ProjectView>>>,
        done: impl FnMut(&Option<Arc<ProjectView>>) -> bool,
    ) {
        let waited = tokio::time::timeout(Duration::from_secs(5), view.wait_for(done)).await;
        waited.expect("the view settles within 5 s").expect("the actor is running");
    }

    #[tokio::test]
    async fn a_bad_address_is_refused_with_the_parsers_message_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(dir.path(), [("local", FakeT3::new(seirei_code()))]);
        let refused = daemon.call(add("studio", "https://example.com")).await.expect_err("not a tailnet address");
        let parser = TailnetOrigin::parse("https://example.com").expect_err("the parser refuses it too");
        assert_eq!(refused.message, parser.to_string());
        assert!(!dir.path().join("config/machines.toml").exists(), "nothing was written");
        assert_eq!(statuses(&daemon).await.len(), 1, "and nothing was added");
    }

    #[tokio::test]
    async fn a_login_swaps_the_credential_on_the_machines_own_connection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(dir.path(), [("local", FakeT3::new(seirei_code()))]);
        assert_eq!(daemon.call(add("studio", &format!("{STUDIO}/"))).await, Ok(Reply::Done));
        assert_eq!(statuses(&daemon).await[1], ("studio".into(), MachineStatus::NeedsLogin, 0, None));
        let at = ProjectRef { machine: machine("studio"), project: ProjectId::new("fake") };
        let actor = daemon.project(&at).expect("an actor on the added machine");
        let mut view = actor.view();
        settled(&mut view, |v| v.as_ref().is_some_and(|v| v.health.problem == Some(Problem::Unauthorized))).await;

        let codeless = Command::Login { machine: machine("studio"), access: Access::Auto, code: None };
        let refused = daemon.call(codeless).await.expect_err("a remote T3 needs a code made there");
        assert!(refused.message.contains(PAIRING_HINT), "{}", refused.message);
        let connection = fake(&daemon, "studio");
        let login = Command::Login { machine: machine("studio"), access: Access::Auto, code: Some("PAIR-1".into()) };
        assert_eq!(daemon.call(login).await, Ok(Reply::Done));

        assert!(Arc::ptr_eq(&connection, &fake(&daemon, "studio")), "the connection was kept, not replaced");
        assert_eq!(connection.with(|t3| t3.token.clone()).as_deref(), Some("token-PAIR-1"));
        let saved = login::load(&dir.path().join("config/credentials/studio.json")).expect("readable").expect("saved");
        assert_eq!((saved.origin.as_str(), saved.token.as_str(), saved.access), (STUDIO, "token-PAIR-1", Access::Auto));
        let send = ProjectCommand::Send { thread: ThreadId::new("t"), text: "hi".into(), send_id: SendId::fresh() };
        actor.call(send).await.expect("the actor still takes commands");
        settled(&mut view, |v| {
            v.as_ref().is_some_and(|v| v.health.problem.is_none() && v.access == Some(Access::Auto))
        })
        .await;
        let label = connection.with(|t3| t3.environment.label.clone());
        assert_eq!(statuses(&daemon).await[1], ("studio".into(), MachineStatus::Connected, 1, Some(label)));
    }

    #[tokio::test]
    async fn removing_a_machine_stops_its_actors_and_closes_the_feeds_watching_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(dir.path(), [("local", FakeT3::new(seirei_code()))]);
        daemon.call(add("studio", STUDIO)).await.expect("added");
        fake(&daemon, "studio").with(|t3| t3.down = None);
        assert_eq!(statuses(&daemon).await[1].1, MachineStatus::Connected, "its identity is recorded");
        let watched = ProjectRef { machine: machine("studio"), project: ProjectId::new("fake") };
        let mut feeds = Feeds::default();
        feeds.subscribe(&daemon, Subscription { project: Some(watched.clone()), job: None }).await;
        let actor = daemon.projects.lock().unwrap()[&watched].clone();
        let local = daemon.project(&ProjectRef { machine: MachineId::local(), project: seirei_code().id }).unwrap();
        let credential = config::credentials(&dir.path().join("config"), &machine("studio"));
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "{}").unwrap();

        assert_eq!(daemon.call(Command::RemoveMachine { machine: machine("studio") }).await, Ok(Reply::Done));
        assert!(!credential.exists(), "a removed machine's token does not stay on disk");
        tokio::time::timeout(Duration::from_secs(5), actor.stopped()).await.expect("studio's actor stopped");
        let closed = async {
            while feeds.project.is_some() {
                feeds.changed().await;
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), closed).await.expect("the session let go of the project");
        assert_eq!(feeds.subscription, Subscription::default(), "subscribing again starts afresh");
        assert!(tokio::time::timeout(Duration::from_millis(100), local.stopped()).await.is_err(), "local runs on");

        let listed: Vec<String> = statuses(&daemon).await.into_iter().map(|(id, ..)| id).collect();
        assert_eq!(listed, ["local"]);
        assert!(!std::fs::read_to_string(dir.path().join("config/machines.toml")).unwrap().contains("studio"));
        assert!(
            dir.path().join("machines/studio/identity.json").exists(),
            "its identity stays for the duplicate rules"
        );
        let again = daemon.call(Command::RemoveMachine { machine: machine("studio") }).await.expect_err("gone");
        assert!(again.message.contains("no machine is named studio"), "{}", again.message);
        let local_refused = daemon.call(Command::RemoveMachine { machine: MachineId::local() }).await;
        assert!(local_refused.is_err(), "local cannot be removed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_adds_of_different_machines_all_land_in_machines_toml() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(dir.path(), [("local", FakeT3::new(seirei_code()))]);
        let names: Vec<String> = (0..32).map(|i| format!("box-{i:02}")).collect();
        let start = Arc::new(tokio::sync::Barrier::new(names.len()));
        let mut adds = JoinSet::new();
        for name in names.clone() {
            let (daemon, start) = (daemon.clone(), start.clone());
            adds.spawn(async move {
                start.wait().await;
                daemon.call(add(&name, &format!("https://{name}.tail123b88.ts.net"))).await
            });
        }
        for added in adds.join_all().await {
            assert_eq!(added, Ok(Reply::Done));
        }
        let file = std::fs::read_to_string(dir.path().join("config/machines.toml")).expect("written");
        let missing: Vec<&String> = names.iter().filter(|name| !file.contains(&format!("[{name}]"))).collect();
        assert!(missing.is_empty(), "lost from machines.toml: {missing:?}");
        assert_eq!(statuses(&daemon).await.len(), names.len() + 1, "and every one is connected");
    }

    #[tokio::test]
    async fn another_build_is_told_and_disconnected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = daemon(dir.path(), [("local", FakeT3::new(seirei_code()))]);
        let mut client = Client::connect(&daemon);
        client.send(ClientMsg::Hello { build: BuildId("0.0.0+elsewhere".into()), take_over: false }).await;
        assert_eq!(client.recv().await, Some(ServerMsg::WrongBuild { daemon: BuildId::current() }));
        assert_eq!(client.recv().await, None, "the daemon closes the connection");
        assert!(!*daemon.shutdown.borrow(), "the daemon keeps running for its own build's clients");

        let mut usurper = Client::connect(&daemon);
        usurper.send(ClientMsg::Hello { build: BuildId("0.0.0+elsewhere".into()), take_over: true }).await;
        assert_eq!(usurper.recv().await, Some(ServerMsg::Yielding));
        assert!(*daemon.shutdown.borrow(), "a take-over stops the daemon");
    }
}
