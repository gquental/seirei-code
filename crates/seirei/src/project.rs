use crate::coordinator::{self, Binding, CoordinatorKey, EnsureStep, ProjectRecord};
use crate::t3::{PageSize, Position, ReadQuery, T3Api, T3Error, ThreadPage};
use seirei_proto::{
    ids::{ItemId, ProjectId, RequestId, SendId, T3Time, ThreadId},
    model::{
        Answers, CoordinatorView, Health, ModelSelection, PendingRequest, Problem, Project, ProjectView, ThreadStatus,
        ThreadSummary,
    },
    timeline::Timeline,
    wire::{CallError, Reply},
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::{mpsc, oneshot, watch};

const MAX_CHARS: u32 = 4_000;
/// Rows re-read from the previous page. A thread archived mid-scan shifts later rows up by one;
/// the overlap keeps them from falling between pages.
const SCAN_OVERLAP: u64 = 5;
const WATCHED: Duration = Duration::from_millis(1500);
const UNWATCHED: Duration = Duration::from_secs(20);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ProjectHandle {
    inbox: mpsc::Sender<Msg>,
    view: Arc<watch::Sender<Option<Arc<ProjectView>>>>,
}

pub enum ProjectCommand {
    EnsureCoordinator,
    Send { thread: ThreadId, text: String, send_id: SendId },
    Answer { request: RequestId, answers: Answers },
}

impl ProjectCommand {
    fn has_retry_key(&self) -> bool {
        matches!(self, Self::Send { .. })
    }
}

enum Msg {
    Call(ProjectCommand, oneshot::Sender<Result<Reply, CallError>>),
    OpenTimeline(ThreadId, oneshot::Sender<Option<watch::Receiver<Arc<Timeline>>>>),
}

impl ProjectHandle {
    pub fn spawn<A: T3Api>(
        api: Arc<A>,
        project: ProjectId,
        state_dir: PathBuf,
        coordinator_model: Option<ModelSelection>,
    ) -> io::Result<Self> {
        let view = Arc::new(watch::Sender::new(None));
        let actor = Actor::new(api, project, state_dir, coordinator_model, view.clone())?;
        let (inbox, messages) = mpsc::channel(32);
        tokio::spawn(actor.run(messages));
        Ok(Self { inbox, view })
    }

    pub fn view(&self) -> watch::Receiver<Option<Arc<ProjectView>>> {
        self.view.subscribe()
    }

    pub async fn open_timeline(&self, thread: ThreadId) -> Option<watch::Receiver<Arc<Timeline>>> {
        let (reply, rx) = oneshot::channel();
        self.inbox.send(Msg::OpenTimeline(thread, reply)).await.ok()?;
        rx.await.ok()?
    }

    pub async fn call(&self, command: ProjectCommand) -> Result<Reply, CallError> {
        let stopped = |retry_safe| CallError { message: "the project's sync task stopped".into(), retry_safe };
        let keyed = command.has_retry_key();
        let (reply, rx) = oneshot::channel();
        self.inbox.send(Msg::Call(command, reply)).await.map_err(|_| stopped(true))?;
        rx.await.unwrap_or_else(|_| Err(stopped(keyed)))
    }
}

struct Actor<A> {
    api: Arc<A>,
    id: ProjectId,
    state_dir: PathBuf,
    coordinator_model: Option<ModelSelection>,
    record: ProjectRecord,
    project: Option<Project>,
    threads: BTreeMap<ThreadId, ThreadSummary>,
    hot: HashMap<ThreadId, Hot>,
    questions: Vec<PendingRequest>,
    horizon: T3Time,
    looked_up: bool,
    launching: bool,
    health: Health,
    /// List rows that failed to parse the last time a scan read them.
    unparsed: BTreeMap<ThreadId, String>,
    failures: u32,
    view: Arc<watch::Sender<Option<Arc<ProjectView>>>>,
}

struct Hot {
    /// The thread's `updatedAt` when this window was last read. Every timeline event bumps
    /// `updatedAt`, so a refresh is due exactly when the list reports a different one.
    synced_at: Option<T3Time>,
    tx: watch::Sender<Arc<Timeline>>,
    unparsed: BTreeMap<ItemId, String>,
}

impl<A: T3Api> Actor<A> {
    fn new(
        api: Arc<A>,
        id: ProjectId,
        state_dir: PathBuf,
        coordinator_model: Option<ModelSelection>,
        view: Arc<watch::Sender<Option<Arc<ProjectView>>>>,
    ) -> io::Result<Self> {
        let record = ProjectRecord::load(&state_dir, &id)?;
        let horizon = record.binding.as_ref().map_or(T3Time(0), |binding| binding.minted_after);
        Ok(Self {
            api,
            id,
            state_dir,
            coordinator_model,
            record,
            project: None,
            threads: BTreeMap::new(),
            hot: HashMap::new(),
            questions: Vec::new(),
            horizon,
            looked_up: false,
            launching: false,
            health: Health { last_sync: None, problem: None },
            unparsed: BTreeMap::new(),
            failures: 0,
            view,
        })
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<Msg>) {
        let mut next = tokio::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                msg = inbox.recv() => match msg {
                    Some(Msg::Call(command, reply)) => {
                        let _ = reply.send(self.call(command).await);
                        next = tokio::time::Instant::now();
                    }
                    Some(Msg::OpenTimeline(thread, reply)) => {
                        let hot = self.hot.len();
                        let _ = reply.send(self.open_timeline(thread));
                        if self.hot.len() > hot {
                            next = tokio::time::Instant::now();
                        }
                    }
                    None => return,
                },
                () = tokio::time::sleep_until(next) => {
                    let outcome = self.tick().await;
                    self.record_outcome(outcome);
                    next = tokio::time::Instant::now() + cadence(self.view.receiver_count(), self.failures);
                }
            }
        }
    }

    async fn tick(&mut self) -> Result<(), Failure> {
        self.hot.retain(|_, hot| hot.tx.receiver_count() > 0);
        if self.project.is_none() {
            self.fetch_project().await?;
        }
        self.adopt_lost_launch().await?;
        if self.record.binding.as_ref().is_some_and(|b| b.thread.is_some()) {
            self.scan().await?;
        }
        let due: Vec<ThreadId> = (self.hot.iter())
            .filter(|(id, hot)| self.threads.get(*id).is_some_and(|row| hot.synced_at != Some(row.updated_at)))
            .map(|(id, _)| id.clone())
            .collect();
        for thread in due {
            self.refresh_timeline(&thread).await?;
        }
        self.refresh_questions().await
    }

    async fn fetch_project(&mut self) -> Result<Project, Failure> {
        let found = self.api.projects().await?.into_iter().find(|project| project.id == self.id);
        let project = found.ok_or_else(|| Failure::NoProject(self.id.clone()))?;
        self.project = Some(project.clone());
        Ok(project)
    }

    async fn scan(&mut self) -> Result<(), Failure> {
        let mut cursor = None;
        let mut newest = None;
        loop {
            let ThreadPage { threads, next, unparsed } = self.api.threads(&self.id, cursor).await?;
            newest = newest.or(threads.first().map(|t| t.updated_at));
            let reached = reaches_horizon(&threads, self.horizon);
            self.unparsed.extend(unparsed);
            for row in threads {
                self.unparsed.remove(&row.id);
                upsert(&mut self.threads, row);
            }
            match next {
                Some(next) if !reached => cursor = Some(next.saturating_sub(SCAN_OVERLAP)),
                _ => break,
            }
        }
        if let Some(newest) = newest {
            self.horizon = self.horizon.max(newest);
        }
        Ok(())
    }

    async fn refresh_timeline(&mut self, thread: &ThreadId) -> Result<(), Failure> {
        let mut after = tail_after(self.threads[thread].item_count);
        let mut page = self.api.read(thread, tail_query(after)).await?;
        let current = tail_after(page.detail.summary.item_count);
        if current != after {
            after = current;
            page = self.api.read(thread, tail_query(after)).await?;
        }
        let summary = page.detail.summary.clone();
        let timeline =
            Timeline { thread: thread.clone(), earlier: page.earlier, items: page.items, detail: Some(page.detail) };
        let hot = self.hot.get_mut(thread).expect("only hot timelines are refreshed");
        hot.synced_at = (!page.has_more).then_some(summary.updated_at);
        hot.unparsed = page.unparsed;
        hot.tx.send_if_modified(|current| {
            let changed = **current != timeline;
            if changed {
                *current = Arc::new(timeline);
            }
            changed
        });
        upsert(&mut self.threads, summary);
        Ok(())
    }

    async fn refresh_questions(&mut self) -> Result<(), Failure> {
        let waiting: Vec<ThreadId> = (self.members().into_iter())
            .filter(|id| self.threads.get(id).is_some_and(|row| row.status == ThreadStatus::Waiting))
            .collect();
        self.questions.retain(|question| waiting.contains(&question.thread));
        for thread in waiting {
            let open = self.api.pending(&thread).await?;
            self.questions.retain(|question| question.thread != thread || open.contains(&question.id));
            for request in open {
                if !self.questions.iter().any(|question| question.id == request) {
                    let question = self.api.question(&thread, &request).await?;
                    self.questions.push(question);
                }
            }
        }
        Ok(())
    }

    fn open_timeline(&mut self, thread: ThreadId) -> Option<watch::Receiver<Arc<Timeline>>> {
        if let Some(hot) = self.hot.get(&thread) {
            return Some(hot.tx.subscribe());
        }
        if !self.members().contains(&thread) {
            return None;
        }
        let (tx, rx) = watch::channel(Arc::new(Timeline::empty(thread.clone())));
        self.hot.insert(thread, Hot { synced_at: None, tx, unparsed: BTreeMap::new() });
        Some(rx)
    }

    fn members(&self) -> Vec<ThreadId> {
        let Some(Binding { key, thread: Some(coordinator), .. }) = &self.record.binding else { return Vec::new() };
        let jobs = coordinator::jobs(&self.threads, coordinator, key);
        let subtasks = jobs.iter().flat_map(|job| job.subtasks.iter().map(|t| t.id.clone()));
        let mut members: Vec<ThreadId> = jobs.iter().map(|job| job.thread.id.clone()).chain(subtasks).collect();
        members.push(coordinator.clone());
        members
    }

    async fn call(&mut self, command: ProjectCommand) -> Result<Reply, CallError> {
        let keyed = command.has_retry_key();
        let outcome = match command {
            ProjectCommand::EnsureCoordinator => self.ensure_coordinator().await.map(drop),
            ProjectCommand::Send { thread, text, send_id } => {
                self.api.send(&thread, &text, &send_id).await.map_err(Failure::from)
            }
            ProjectCommand::Answer { request, answers } => self.answer(request, answers).await,
        };
        let failed = |e: Failure| CallError { retry_safe: keyed || e.retry_safe(), message: e.to_string() };
        outcome.map(|()| Reply::Done).map_err(failed)
    }

    async fn answer(&mut self, request: RequestId, answers: Answers) -> Result<(), Failure> {
        let question = (self.questions.iter().find(|question| question.id == request))
            .ok_or_else(|| Failure::NotPending(request.clone()))?;
        self.api.answer(question, &answers).await?;
        self.questions.retain(|question| question.id != request);
        Ok(())
    }

    async fn adopt_lost_launch(&mut self) -> Result<(), Failure> {
        if coordinator::ensure_step(self.record.binding.as_ref(), self.looked_up) == EnsureStep::Lookup {
            self.lookup().await?;
        }
        Ok(())
    }

    async fn lookup(&mut self) -> Result<(), Failure> {
        let binding = self.record.binding.clone().expect("only a key without a thread is looked up");
        let tagged = self.api.threads_titled(&self.id, &binding.key.coordinator_tag()).await?;
        match coordinator::adopt(&tagged, &binding.key) {
            Some(found) => self.bind(Binding { thread: Some(found.id.clone()), ..binding })?,
            None => self.looked_up = true,
        }
        Ok(())
    }

    async fn ensure_coordinator(&mut self) -> Result<ThreadId, Failure> {
        self.looked_up = false;
        loop {
            match coordinator::ensure_step(self.record.binding.as_ref(), self.looked_up) {
                EnsureStep::Ready(thread) => return Ok(thread),
                EnsureStep::Mint => {
                    let newest = self.api.threads(&self.id, None).await?.threads.first().map(|t| t.updated_at);
                    let key = CoordinatorKey::mint();
                    self.bind(Binding { key, minted_after: newest.unwrap_or(T3Time(0)), thread: None })?;
                    self.looked_up = true;
                }
                EnsureStep::Lookup => self.lookup().await?,
                EnsureStep::Launch => {
                    let binding = self.record.binding.clone().expect("Launch implies a binding");
                    let project = self.fetch_project().await?;
                    let newest = self.api.threads(&self.id, None).await?.threads.into_iter().next();
                    let runnable = self.api.models().await?;
                    let configured = self.coordinator_model.as_ref();
                    let model = coordinator::launch_model(configured, &project, newest.as_ref(), &runnable)?;
                    self.looked_up = false;
                    self.launching = true;
                    self.publish();
                    let launched = self.api.launch(coordinator::launch(&project, &binding.key, model)).await;
                    self.launching = false;
                    self.bind(Binding { thread: Some(launched?), ..binding })?;
                }
            }
        }
    }

    fn bind(&mut self, binding: Binding) -> Result<(), Failure> {
        self.horizon = binding.minted_after;
        self.record.binding = Some(binding);
        Ok(self.record.save(&self.state_dir, &self.id)?)
    }

    fn record_outcome(&mut self, outcome: Result<(), Failure>) {
        match outcome {
            Ok(()) => {
                self.failures = 0;
                let mut unparsed =
                    self.unparsed.values().chain(self.hot.values().flat_map(|hot| hot.unparsed.values()));
                let problem = unparsed
                    .next()
                    .map(|sample| Problem::Unparsed { sample: sample.clone(), count: 1 + unparsed.count() });
                self.health = Health { last_sync: Some(SystemTime::now()), problem };
            }
            Err(failure) => {
                self.failures += 1;
                self.health.problem = Some(failure.problem());
            }
        }
        self.publish();
    }

    fn publish(&mut self) {
        let project = (self.project.clone()).unwrap_or_else(|| Project {
            id: self.id.clone(),
            title: String::new(),
            root: PathBuf::new(),
            default_model: None,
        });
        let (coordinator, jobs) = match &self.record.binding {
            Some(Binding { key, thread: Some(id), .. }) => match self.threads.get(id) {
                Some(row) => (CoordinatorView::Ready(row.clone()), coordinator::jobs(&self.threads, id, key)),
                None => (CoordinatorView::Starting, Vec::new()),
            },
            Some(Binding { thread: None, .. }) if self.launching => (CoordinatorView::Starting, Vec::new()),
            None | Some(Binding { thread: None, .. }) => (CoordinatorView::Absent, Vec::new()),
        };
        let view = ProjectView {
            project,
            coordinator,
            jobs,
            questions: self.questions.clone(),
            health: self.health.clone(),
            access: self.api.access(),
        };
        self.view.send_if_modified(|current| {
            let changed = current.as_deref() != Some(&view);
            if changed {
                *current = Some(Arc::new(view));
            }
            changed
        });
    }
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    T3(#[from] T3Error),
    #[error("saving the project record: {0}")]
    Record(#[from] std::io::Error),
    #[error("T3 has no project {}", .0.as_str())]
    NoProject(ProjectId),
    #[error("that question is no longer pending")]
    NotPending(RequestId),
    #[error(transparent)]
    NoModel(#[from] coordinator::NoModel),
}

impl Failure {
    fn retry_safe(&self) -> bool {
        match self {
            Self::T3(e) => !e.may_have_applied(),
            Self::Record(_) | Self::NoProject(_) | Self::NotPending(_) | Self::NoModel(_) => true,
        }
    }

    fn problem(&self) -> Problem {
        let failed = || Problem::Failed { detail: self.to_string() };
        match self {
            Self::T3(e) => t3_problem(e).unwrap_or_else(failed),
            Self::Record(_) | Self::NoProject(_) | Self::NotPending(_) | Self::NoModel(_) => failed(),
        }
    }
}

fn t3_problem(e: &T3Error) -> Option<Problem> {
    match e {
        T3Error::Unreachable(cause) | T3Error::Lost(cause) => Some(Problem::Unreachable { detail: cause.clone() }),
        T3Error::Unauthorized => Some(Problem::Unauthorized),
        T3Error::NoSession(cause) => t3_problem(cause),
        T3Error::Rejected(_) | T3Error::Malformed(_) => None,
    }
}

fn reaches_horizon(page: &[ThreadSummary], horizon: T3Time) -> bool {
    page.last().is_none_or(|oldest| oldest.updated_at < horizon)
}

fn tail_after(item_count: u64) -> Option<Position> {
    item_count.checked_sub(u64::from(PageSize::MAX.get()) + 1).map(Position)
}

fn tail_query(after: Option<Position>) -> ReadQuery {
    ReadQuery { after, limit: PageSize::MAX, max_chars: MAX_CHARS }
}

fn upsert(threads: &mut BTreeMap<ThreadId, ThreadSummary>, row: ThreadSummary) {
    if threads.get(&row.id).is_none_or(|held| held.updated_at <= row.updated_at) {
        threads.insert(row.id.clone(), row);
    }
}

fn cadence(watchers: usize, failures: u32) -> Duration {
    let base = if watchers > 0 { WATCHED } else { UNWATCHED };
    (base * 2u32.pow(failures.min(6))).min(MAX_BACKOFF.max(base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::t3::fake::{FakeT3, State, Write, opus};
    use seirei_proto::{
        ids::{ModelId, ProviderInstanceId},
        model::{Access, Choice, JobLink, Lineage, Question, RuntimeMode},
    };
    use std::path::Path;

    fn seirei_code() -> Project {
        Project {
            id: ProjectId::new("480923ae-52f3-431e-a558-42c44e1b9fb3"),
            title: "seirei-code".into(),
            root: "/home/gquental/.t3/projects/seirei-code".into(),
            default_model: None,
        }
    }

    fn actor(api: &Arc<FakeT3>, state_dir: &Path) -> Actor<FakeT3> {
        configured_actor(api, state_dir, None)
    }

    fn configured_actor(
        api: &Arc<FakeT3>,
        state_dir: &Path,
        coordinator_model: Option<ModelSelection>,
    ) -> Actor<FakeT3> {
        let view = Arc::new(watch::Sender::new(None));
        Actor::new(api.clone(), seirei_code().id, state_dir.to_path_buf(), coordinator_model, view)
            .expect("the record loads")
    }

    async fn sync(actor: &mut Actor<FakeT3>) -> Arc<ProjectView> {
        let outcome = actor.tick().await;
        assert!(outcome.is_ok(), "the tick succeeds: {outcome:?}");
        actor.record_outcome(outcome);
        actor.view.borrow().clone().expect("a tick that fetched the project publishes")
    }

    fn ready(view: &ProjectView) -> &ThreadSummary {
        match &view.coordinator {
            CoordinatorView::Ready(thread) => thread,
            other => panic!("expected a ready coordinator, got {other:?}"),
        }
    }

    async fn with_coordinator(api: &Arc<FakeT3>, state_dir: &Path) -> (Actor<FakeT3>, ThreadId) {
        let mut actor = actor(api, state_dir);
        sync(&mut actor).await;
        assert_eq!(actor.call(ProjectCommand::EnsureCoordinator).await, Ok(Reply::Done));
        let coordinator = ready(&*sync(&mut actor).await).id.clone();
        (actor, coordinator)
    }

    #[tokio::test]
    async fn ensuring_from_nothing_launches_once_and_becomes_ready() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        api.with(|t3| t3.add("earlier", "Earlier work", Lineage::Root { via_mcp: false }));
        let dir = tempfile::tempdir().expect("temp dir");
        let mut actor = actor(&api, dir.path());
        assert_eq!(sync(&mut actor).await.coordinator, CoordinatorView::Absent);

        api.with(|t3| t3.watching = Some(actor.view.subscribe()));
        assert_eq!(actor.call(ProjectCommand::EnsureCoordinator).await, Ok(Reply::Done));
        let shown = api.with(|t3| t3.shown_at_launch.clone());
        assert_eq!(shown, [Some(CoordinatorView::Starting)], "the pane shows the launch while it is in flight");
        let view = sync(&mut actor).await;
        let coordinator = ready(&view);
        assert_eq!(api.with(|t3| t3.launched.clone()), std::slice::from_ref(&coordinator.id), "exactly one launch");
        let key = actor.record.binding.as_ref().expect("bound").key.clone();
        assert!(coordinator.title.contains(&key.coordinator_tag()), "the title carries the key: {}", coordinator.title);

        assert_eq!(actor.call(ProjectCommand::EnsureCoordinator).await, Ok(Reply::Done));
        assert_eq!(api.with(|t3| t3.launched.len()), 1, "ensuring again is a no-op");
        let saved = ProjectRecord::load(dir.path(), &seirei_code().id).expect("the record is readable");
        assert_eq!(saved.binding.and_then(|b| b.thread), Some(coordinator.id.clone()), "the binding is durable");
    }

    #[tokio::test]
    async fn a_launch_lost_before_it_was_recorded_is_adopted_after_a_restart() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        api.with(|t3| t3.lose_next = Some(Write::Launch));
        let dir = tempfile::tempdir().expect("temp dir");
        let mut crashed = actor(&api, dir.path());
        sync(&mut crashed).await;
        let lost = crashed.call(ProjectCommand::EnsureCoordinator).await.expect_err("the launch answer was lost");
        assert!(!lost.retry_safe, "a launch whose answer was lost may have applied");
        crashed.publish();
        let shown = crashed.view.borrow().as_ref().map(|view| view.coordinator.clone());
        assert_eq!(shown, Some(CoordinatorView::Absent), "nothing is starting once the launch call returned");
        drop(crashed);

        let mut restarted = actor(&api, dir.path());
        let view = sync(&mut restarted).await;
        let launched = api.with(|t3| t3.launched.clone());
        assert_eq!(launched.len(), 1, "the restart adopted the tagged thread instead of launching again");
        assert_eq!(ready(&view).id, launched[0]);
        let saved = ProjectRecord::load(dir.path(), &seirei_code().id).expect("the record is readable");
        assert_eq!(saved.binding.and_then(|b| b.thread), Some(launched[0].clone()));
    }

    #[tokio::test]
    async fn a_refused_launch_fails_the_command_and_no_tick_retries_it() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let refusal = "runtime_mode_escalation_denied: the coordinator would run above this login";
        api.with(|t3| t3.refuse_launches = Some(refusal.into()));
        let dir = tempfile::tempdir().expect("temp dir");
        let mut actor = actor(&api, dir.path());
        sync(&mut actor).await;

        let refused = actor.call(ProjectCommand::EnsureCoordinator).await.expect_err("T3 refused the launch");
        assert!(refused.message.contains(refusal), "the reply carries T3's reason: {}", refused.message);
        assert!(refused.retry_safe, "a refused launch applied nothing");
        for _ in 0..3 {
            let view = sync(&mut actor).await;
            assert_eq!(view.coordinator, CoordinatorView::Absent);
            assert_eq!(view.health.problem, None, "a refused launch fails the command, not the sync");
        }
        assert_eq!(api.with(|t3| t3.launch_calls), 1, "no tick launches");

        let key = actor.record.binding.as_ref().expect("the key stays").key.clone();
        let title = format!("Coordinator · seirei-code [{}]", key.coordinator_tag());
        let late = api.with(|t3| {
            t3.refuse_launches = None;
            t3.add("mcp:late", &title, Lineage::Root { via_mcp: true })
        });
        assert_eq!(sync(&mut actor).await.coordinator, CoordinatorView::Absent, "ticks look once per launch");
        assert_eq!(actor.call(ProjectCommand::EnsureCoordinator).await, Ok(Reply::Done));
        assert_eq!(api.with(|t3| t3.launch_calls), 1, "creating again looked first and adopted the tagged thread");
        let binding = actor.record.binding.clone().expect("bound");
        assert_eq!((binding.key, binding.thread), (key, Some(late)), "the key was reused");
    }

    #[tokio::test]
    async fn the_coordinator_launches_on_the_configured_default_newest_or_first_runnable_model() {
        let selection = |provider: &str, model: &str| ModelSelection {
            provider: ProviderInstanceId::new(provider),
            model: ModelId::new(model),
        };
        let (grok, luna, sol) =
            (selection("grok", "grok-4.7"), selection("codex", "gpt-6-luna"), selection("codex", "gpt-6.1-sol"));
        let launched_on = async |configured: Option<ModelSelection>, setup: &dyn Fn(&mut State)| {
            let api = Arc::new(FakeT3::new(seirei_code()));
            let dir = tempfile::tempdir().expect("temp dir");
            let mut actor = configured_actor(&api, dir.path(), configured);
            sync(&mut actor).await;
            api.with(setup);
            let reply = actor.call(ProjectCommand::EnsureCoordinator).await;
            reply.map(|_| api.with(|t3| t3.thread_mut(&t3.launched[0].clone()).model.clone()))
        };

        let newest = |t3: &mut State| {
            let thread = t3.add("recent", "Recent work", Lineage::Root { via_mcp: false });
            t3.thread_mut(&thread).model = grok.clone();
        };
        assert_eq!(launched_on(None, &newest).await, Ok(grok.clone()), "no default: the newest thread's model");
        let defaulted = |t3: &mut State| {
            newest(t3);
            t3.project.default_model = Some(luna.clone());
        };
        assert_eq!(launched_on(None, &defaulted).await, Ok(luna.clone()), "a default set after the first sync applies");
        assert_eq!(launched_on(Some(sol.clone()), &defaulted).await, Ok(sol), "the config outranks the default");
        assert_eq!(launched_on(None, &|_| {}).await, Ok(opus()), "an empty project takes the first runnable model");

        let failed = launched_on(None, &|t3| t3.models.clear()).await.expect_err("nothing can run");
        assert!(failed.message.contains("no model to run the coordinator on"), "{}", failed.message);
        assert!(failed.retry_safe);
    }

    #[tokio::test]
    async fn a_project_t3_does_not_list_still_gets_a_view_that_says_why() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let dir = tempfile::tempdir().expect("temp dir");
        let gone = ProjectId::new("mcp:gone");
        let mut actor =
            Actor::new(api, gone.clone(), dir.path().to_path_buf(), None, Arc::new(watch::Sender::new(None)))
                .expect("no record to load");
        let outcome = actor.tick().await;
        assert!(matches!(outcome, Err(Failure::NoProject(_))), "{outcome:?}");
        actor.record_outcome(outcome);

        let view = actor.view.borrow().clone().expect("a failed tick publishes");
        let placeholder = Project { id: gone, title: String::new(), root: PathBuf::new(), default_model: None };
        assert_eq!(view.project, placeholder, "a project never fetched is named by its id alone");
        let detail = "T3 has no project mcp:gone".to_owned();
        assert_eq!(view.health.problem, Some(Problem::Failed { detail }));
    }

    #[tokio::test]
    async fn the_view_carries_the_credentials_grant() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let dir = tempfile::tempdir().expect("temp dir");
        let mut actor = actor(&api, dir.path());
        assert_eq!(sync(&mut actor).await.access, None, "a token from the environment has no known grant");
        api.with(|t3| t3.access = Some(Access::Auto));
        assert_eq!(sync(&mut actor).await.access, Some(Access::Auto));
    }

    #[tokio::test]
    async fn the_scan_pages_back_to_the_horizon_and_derives_jobs() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        api.with(|t3| {
            (0..150).for_each(|i| drop(t3.add(&format!("before-{i:03}"), "Older", Lineage::Root { via_mcp: false })))
        });
        let dir = tempfile::tempdir().expect("temp dir");
        let (bound, coordinator) = with_coordinator(&api, dir.path()).await;
        let job_tag = bound.record.binding.as_ref().expect("bound").key.job_tag();
        drop(bound);

        api.with(|t3| {
            let job = t3.add("mcp:job-a", &format!("Add the cache [{job_tag}]"), Lineage::Root { via_mcp: true });
            for i in 0..110 {
                t3.add(&format!("after-{i:03}"), "Unrelated", Lineage::Root { via_mcp: false });
            }
            let helper = t3.add("helper", "Read the cache", Lineage::Subagent { parent: job });
            t3.add("helper-of-helper", "Grep for callers", Lineage::Subagent { parent: helper });
            t3.add("sub-job", "Review the plan", Lineage::Subagent { parent: coordinator.clone() });
            t3.thread_mut(&ThreadId::new("sub-job")).status = ThreadStatus::Idle;
            t3.add("mcp:other", "Not ours [job:OTHERKEY22]", Lineage::Root { via_mcp: true });
            t3.list_calls = 0;
        });
        let mut actor = actor(&api, dir.path());
        let view = sync(&mut actor).await;
        assert_eq!(api.with(|t3| t3.list_calls), 2, "a cold scan pages back to the key's minting, not further");
        let shape: Vec<(&str, JobLink, Vec<&str>)> = (view.jobs.iter())
            .map(|job| (job.thread.id.as_str(), job.link, job.subtasks.iter().map(|t| t.id.as_str()).collect()))
            .collect();
        assert_eq!(
            shape,
            [
                ("mcp:job-a", JobLink::Launched, vec!["helper-of-helper", "helper"]),
                ("sub-job", JobLink::Subagent, vec![]),
            ],
            "subtasks seen before their job are attributed, and the live job sorts first"
        );
        assert!(!actor.threads.contains_key(&ThreadId::new("before-000")), "rows older than the key are never read");

        api.with(|t3| t3.list_calls = 0);
        sync(&mut actor).await;
        assert_eq!(api.with(|t3| t3.list_calls), 1, "a warm scan reads one page");
    }

    #[tokio::test]
    async fn a_refreshed_window_is_exactly_t3s_tail_after_an_insertion() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut actor, coordinator) = with_coordinator(&api, dir.path()).await;
        api.with(|t3| (1..150).for_each(|i| t3.insert_item(&coordinator, i, &format!("item-{i:03}"))));
        let timeline = actor.open_timeline(coordinator.clone()).expect("the coordinator's timeline opens");
        let tail = || {
            let ids = api.with(|t3| t3.item_ids(&coordinator));
            ids[ids.len() - usize::from(PageSize::MAX.get())..].to_vec()
        };
        let window = |timeline: &watch::Receiver<Arc<Timeline>>| -> (u64, Vec<ItemId>) {
            let current = timeline.borrow();
            (current.earlier, current.items.iter().map(|item| item.id.clone()).collect())
        };

        sync(&mut actor).await;
        assert_eq!(window(&timeline), (50, tail()), "the first read is the newest 100 of 150 items");

        api.with(|t3| t3.insert_item(&coordinator, 120, "inserted-mid-thread"));
        sync(&mut actor).await;
        let (earlier, items) = window(&timeline);
        assert_eq!(items, tail(), "the window equals T3's tail, with no duplicate and no gap");
        assert_eq!(earlier, 51);
        assert!(items.contains(&ItemId::new("inserted-mid-thread")));

        assert!(actor.open_timeline(ThreadId::new("stranger")).is_none(), "only members have timelines");
    }

    #[tokio::test]
    async fn a_send_carries_the_clients_send_id() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut actor, coordinator) = with_coordinator(&api, dir.path()).await;
        let send_id = SendId::fresh();
        let send = ProjectCommand::Send {
            thread: coordinator.clone(),
            text: "Plan the cache".into(),
            send_id: send_id.clone(),
        };
        assert_eq!(actor.call(send).await, Ok(Reply::Done));
        assert_eq!(api.with(|t3| t3.sends.clone()), [(coordinator, "Plan the cache".to_owned(), send_id)]);
    }

    #[tokio::test]
    async fn questions_from_waiting_members_appear_until_answered() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut actor, coordinator) = with_coordinator(&api, dir.path()).await;
        let request = api.with(|t3| ask(t3, &coordinator));
        let view = sync(&mut actor).await;
        assert_eq!(view.questions.iter().map(|q| &q.id).collect::<Vec<_>>(), [&request]);

        let answer = ProjectCommand::Answer { request: request.clone(), answers: sqlite() };
        assert_eq!(actor.call(answer).await, Ok(Reply::Done));
        assert!(sync(&mut actor).await.questions.is_empty(), "an answered question leaves the view");
        let again = actor.call(ProjectCommand::Answer { request, answers: sqlite() }).await.expect_err("answered");
        assert!(again.retry_safe);
    }

    #[tokio::test]
    async fn a_lost_send_is_safe_to_resend_and_a_lost_answer_is_not() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut actor, coordinator) = with_coordinator(&api, dir.path()).await;
        let send = ProjectCommand::Send { thread: coordinator.clone(), text: "Plan".into(), send_id: SendId::fresh() };
        api.with(|t3| t3.lose_next = Some(Write::Send));
        let lost = actor.call(send).await.expect_err("the send's response was lost");
        assert!(lost.retry_safe, "T3 drops a resend under the same SendId: {}", lost.message);

        let request = api.with(|t3| ask(t3, &coordinator));
        sync(&mut actor).await;
        api.with(|t3| t3.lose_next = Some(Write::Answer));
        let answer = ProjectCommand::Answer { request, answers: sqlite() };
        let lost = actor.call(answer).await.expect_err("the answer's response was lost");
        assert!(!lost.retry_safe, "an answer has no retry key and may have been recorded");
    }

    #[tokio::test]
    async fn an_unparsed_record_is_reported_until_it_parses_or_leaves_what_was_read() {
        let api = Arc::new(FakeT3::new(seirei_code()));
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut actor, coordinator) = with_coordinator(&api, dir.path()).await;
        let unparsed = async |actor: &mut Actor<FakeT3>| match sync(actor).await.health.problem.clone() {
            Some(Problem::Unparsed { count, sample }) => (count, sample),
            None => (0, String::new()),
            Some(other) => panic!("expected only unparsed records, got {other:?}"),
        };

        let odd = api.with(|t3| {
            t3.unreadable.insert("mcp:odd".into());
            t3.add("mcp:odd", "Odd", Lineage::Root { via_mcp: true })
        });
        assert_eq!(unparsed(&mut actor).await, (1, "thread mcp:odd".into()));
        assert_eq!(unparsed(&mut actor).await.0, 1, "a row read twice counts once");
        api.with(|t3| {
            t3.unreadable.clear();
            let at = t3.now();
            t3.thread_mut(&odd).updated_at = at;
        });
        assert_eq!(unparsed(&mut actor).await.0, 0, "a row that parses on a later scan is no longer reported");

        let timeline = actor.open_timeline(coordinator.clone()).expect("the coordinator's timeline opens");
        api.with(|t3| {
            t3.unreadable.insert("odd-item".into());
            t3.insert_item(&coordinator, 1, "odd-item");
        });
        assert_eq!(unparsed(&mut actor).await, (1, "item odd-item".into()));
        api.with(|t3| (2..=101).for_each(|i| t3.insert_item(&coordinator, i, &format!("item-{i:03}"))));
        assert_eq!(unparsed(&mut actor).await.0, 0, "an item that scrolls out of the window is no longer reported");

        api.with(|t3| {
            t3.unreadable.insert("odd-again".into());
            t3.insert_item(&coordinator, 102, "odd-again");
        });
        assert_eq!(unparsed(&mut actor).await.0, 1);
        drop(timeline);
        assert_eq!(unparsed(&mut actor).await.0, 0, "a closed timeline's items are no longer reported");
    }

    fn ask(t3: &mut State, thread: &ThreadId) -> RequestId {
        let request = RequestId::new("request:ask-01");
        t3.questions.push(PendingRequest {
            id: request.clone(),
            thread: thread.clone(),
            questions: vec![Question {
                id: "backend".into(),
                header: "Backend".into(),
                prompt: "Which cache backend?".into(),
                options: vec![Choice { label: "SQLite".into(), description: String::new(), value: None }],
                multi_select: false,
                allow_custom: true,
                required: true,
            }],
        });
        let at = t3.now();
        let row = t3.thread_mut(thread);
        (row.status, row.updated_at) = (ThreadStatus::Waiting, at);
        request
    }

    fn sqlite() -> Answers {
        Answers([("backend".into(), seirei_proto::model::Answer::Picked(vec![0]))].into())
    }

    fn row(updated_at: i64) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId::new(format!("t{updated_at}")),
            title: String::new(),
            status: ThreadStatus::Idle,
            lineage: Lineage::Root { via_mcp: false },
            runtime_mode: RuntimeMode::FullAccess,
            model: opus(),
            item_count: 0,
            settled: false,
            created_at: T3Time(0),
            updated_at: T3Time(updated_at),
        }
    }

    #[test]
    fn only_a_t3_that_never_answered_is_unreachable() {
        for (unreachable, cause) in [
            (T3Error::Unreachable("connection refused".into()), "connection refused"),
            (T3Error::Lost("reset".into()), "reset"),
        ] {
            let detail = cause.to_owned();
            assert_eq!(Failure::from(unreachable).problem(), Problem::Unreachable { detail }, "the cause alone");
        }
        let session_lost = T3Error::NoSession(Box::new(T3Error::Lost("reset".into())));
        assert_eq!(Failure::from(session_lost).problem(), Problem::Unreachable { detail: "reset".into() });
        assert_eq!(Failure::from(T3Error::Unauthorized).problem(), Problem::Unauthorized);
        for failed in [
            Failure::from(T3Error::Rejected("thread_not_found".into())),
            Failure::from(T3Error::Malformed("t3_thread_list: missing threads".into())),
            Failure::from(T3Error::NoSession(Box::new(T3Error::Malformed("<html>".into())))),
            Failure::from(io::Error::other("no space left on device")),
            Failure::NoProject(seirei_code().id),
        ] {
            assert_eq!(failed.problem(), Problem::Failed { detail: failed.to_string() });
        }
    }

    #[test]
    fn scan_stops_at_the_first_page_older_than_the_horizon() {
        assert!(!reaches_horizon(&[row(30), row(20)], T3Time(20)));
        assert!(reaches_horizon(&[row(30), row(19)], T3Time(20)));
        assert!(reaches_horizon(&[], T3Time(20)));
    }

    #[test]
    fn the_tail_read_starts_one_window_before_the_end() {
        assert_eq!(tail_after(0), None);
        assert_eq!(tail_after(100), None);
        assert_eq!(tail_after(101), Some(Position(0)));
        assert_eq!(tail_after(390), Some(Position(289)));
    }
}
