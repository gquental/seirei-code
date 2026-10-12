use crate::coordinator::ProjectRecord;
use crate::project::{ProjectCommand, ProjectHandle};
use crate::t3::T3Api;
use seirei_proto::{
    ids::{ProjectId, ThreadId},
    model::{CoordinatorView, ModelSelection, ProjectView},
    timeline::Timeline,
    wire::{self, BuildId, CallError, ClientMsg, Command, MAX_LINE, Reply, ServerMsg, Subscription},
};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{mpsc, watch},
};

pub struct Daemon<A> {
    api: Arc<A>,
    state_dir: PathBuf,
    coordinator_model: Option<ModelSelection>,
    build: BuildId,
    projects: Mutex<HashMap<ProjectId, ProjectHandle>>,
    shutdown: watch::Sender<bool>,
}

impl<A: T3Api> Daemon<A> {
    pub fn new(api: A, state_dir: PathBuf, coordinator_model: Option<ModelSelection>) -> Arc<Self> {
        Arc::new(Self {
            api: Arc::new(api),
            state_dir,
            coordinator_model,
            build: BuildId::current(),
            projects: Mutex::default(),
            shutdown: watch::Sender::new(false),
        })
    }

    pub fn warm_start(&self) -> io::Result<()> {
        for project in ProjectRecord::saved(&self.state_dir)? {
            if let Err(e) = self.project(&project) {
                eprintln!("seirei: not syncing project {}: {e}", project.as_str());
            }
        }
        Ok(())
    }

    pub fn project(&self, id: &ProjectId) -> io::Result<ProjectHandle> {
        let mut projects = self.projects.lock().expect("no panics while held");
        if let Some(handle) = projects.get(id) {
            return Ok(handle.clone());
        }
        let handle =
            ProjectHandle::spawn(self.api.clone(), id.clone(), self.state_dir.clone(), self.coordinator_model.clone())?;
        projects.insert(id.clone(), handle.clone());
        Ok(handle)
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
        let (project, command) = match command {
            Command::ListProjects => {
                let projects = self.api.projects().await;
                return projects
                    .map(Reply::Projects)
                    .map_err(|e| CallError { message: e.to_string(), retry_safe: true });
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
}

impl Feeds {
    async fn subscribe<A: T3Api>(&mut self, daemon: &Daemon<A>, wanted: Subscription) -> Vec<ServerMsg> {
        if wanted.project != self.subscription.project {
            self.project = wanted.project.as_ref().and_then(|id| match daemon.project(id) {
                Ok(handle) => Some((handle.clone(), handle.view())),
                Err(e) => {
                    eprintln!("seirei: cannot sync project {}: {e}", id.as_str());
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
            view = next_view(&mut self.project) => Change::View(view),
            timeline = next_timeline(&mut self.coordinator) => Change::Timeline(timeline),
            timeline = next_timeline(&mut self.job) => Change::Timeline(timeline),
        };
        match change {
            Change::Timeline(timeline) => vec![ServerMsg::Timeline(timeline)],
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

async fn next_view(feed: &mut Option<(ProjectHandle, watch::Receiver<Option<Arc<ProjectView>>>)>) -> Arc<ProjectView> {
    if let Some((_, view)) = feed {
        while view.changed().await.is_ok() {
            if let Some(current) = view.borrow_and_update().clone() {
                return current;
            }
        }
    }
    std::future::pending().await
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
    use crate::t3::fake::FakeT3;
    use seirei_proto::{
        ids::{SendId, T3Time},
        model::{Lineage, Project},
        wire::{CallId, Command},
    };
    use std::time::Duration;
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

    #[tokio::test]
    async fn a_session_greets_subscribes_and_routes_calls() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = Daemon::new(FakeT3::new(seirei_code()), dir.path().to_path_buf(), None);
        let mut client = Client::connect(&daemon);
        client.send(ClientMsg::Hello { build: BuildId::current(), take_over: false }).await;
        assert_eq!(client.recv().await, Some(ServerMsg::Welcome));

        client.send(ClientMsg::Subscribe(Subscription { project: Some(seirei_code().id), job: None })).await;
        match client.recv().await {
            Some(ServerMsg::Project(view)) => {
                assert_eq!(view.project, seirei_code());
                assert_eq!(view.coordinator, CoordinatorView::Absent);
            }
            other => panic!("the first frame after Subscribe is the project view, got {other:?}"),
        }

        let send_id = SendId::fresh();
        let send = Command::Send {
            project: seirei_code().id,
            thread: ThreadId::new("mcp:coordinator"),
            text: "Plan the cache".into(),
            send_id: send_id.clone(),
        };
        client.send(ClientMsg::Call { id: CallId(7), command: send }).await;
        loop {
            match client.recv().await {
                Some(ServerMsg::Reply { id, result }) => {
                    assert_eq!((id, result), (CallId(7), Ok(Reply::Done)), "the reply answers the call");
                    break;
                }
                Some(ServerMsg::Project(_)) => continue,
                other => panic!("expected the call's reply, got {other:?}"),
            }
        }
        let sent = daemon.api.with(|t3| t3.sends.clone());
        assert_eq!(sent, [(ThreadId::new("mcp:coordinator"), "Plan the cache".to_owned(), send_id)]);
    }

    #[tokio::test]
    async fn a_job_the_actor_does_not_know_never_spins_the_sync_loop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let api = FakeT3::new(seirei_code());
        let coordinator = api.with(|t3| t3.add("mcp:coordinator", "Coordinator", Lineage::Root { via_mcp: true }));
        let binding = Binding { key: CoordinatorKey::mint(), minted_after: T3Time(0), thread: Some(coordinator) };
        ProjectRecord { binding: Some(binding) }.save(dir.path(), &seirei_code().id).expect("record saved");
        let daemon = Daemon::new(api, dir.path().to_path_buf(), None);
        let mut client = Client::connect(&daemon);
        client.send(ClientMsg::Hello { build: BuildId::current(), take_over: false }).await;
        assert_eq!(client.recv().await, Some(ServerMsg::Welcome));

        let wanted = Subscription { project: Some(seirei_code().id), job: Some(ThreadId::new("mcp:stranger")) };
        client.send(ClientMsg::Subscribe(wanted)).await;
        assert!(matches!(client.recv().await, Some(ServerMsg::Project(_))), "the view still streams");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let scans = daemon.api.with(|t3| t3.list_calls);
        assert!(scans <= 2, "retrying the unknown job must not force ticks: {scans} scans in 0.5 s");
    }

    #[tokio::test]
    async fn another_build_is_told_and_disconnected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = Daemon::new(FakeT3::new(seirei_code()), dir.path().to_path_buf(), None);
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
