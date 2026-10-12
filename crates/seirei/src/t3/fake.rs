use super::{
    Environment, Launch, PageSize, Position, ReadQuery, T3Api, T3Error, ThreadPage, TimelinePage,
    login::{Credentials, LoginError, PairingCode},
};
use crate::config::{Endpoint, Token};
use seirei_proto::{
    ids::{ItemId, ModelId, ProjectId, ProviderInstanceId, RequestId, SendId, T3Time, ThreadId},
    model::{
        Access, Answers, CoordinatorView, Lineage, ModelSelection, PendingRequest, Project, ProjectView, RuntimeMode,
        ThreadStatus, ThreadSummary,
    },
    timeline::{Item, ItemKind, ItemStatus, ItemText, ThreadDetail},
};
use std::{
    cmp::Reverse,
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::sync::watch;

const PAGE: usize = PageSize::MAX.get() as usize;

pub struct FakeT3 {
    state: Mutex<State>,
}

pub struct State {
    pub address: String,
    pub project: Project,
    pub access: Option<Access>,
    pub token: Option<String>,
    pub down: Option<Down>,
    /// Unique per fake unless a test makes two share one, as two addresses for one T3 would.
    pub environment: Environment,
    pub models: Vec<ModelSelection>,
    pub threads: Vec<ThreadSummary>,
    pub items: HashMap<ThreadId, Vec<Item>>,
    pub questions: Vec<PendingRequest>,
    pub clock: i64,
    pub list_calls: usize,
    pub launch_calls: usize,
    pub launched: Vec<ThreadId>,
    pub sends: Vec<(ThreadId, String, SendId)>,
    pub lose_next: Option<Write>,
    pub refuse_launches: Option<String>,
    pub watching: Option<watch::Receiver<Option<Arc<ProjectView>>>>,
    pub shown_at_launch: Vec<Option<CoordinatorView>>,
    /// Ids of threads and items reported as unparsed instead of returned.
    pub unreadable: BTreeSet<String>,
}

impl FakeT3 {
    pub fn new(project: Project) -> Self {
        static ENVIRONMENTS: AtomicUsize = AtomicUsize::new(0);
        let n = ENVIRONMENTS.fetch_add(1, Ordering::Relaxed);
        let state = State {
            address: "http://fake.test".into(),
            project,
            access: None,
            token: None,
            down: None,
            environment: Environment { environment_id: format!("environment-{n}"), label: format!("fake-{n}") },
            models: vec![opus()],
            threads: Vec::new(),
            items: HashMap::new(),
            questions: Vec::new(),
            clock: 1_000,
            list_calls: 0,
            launch_calls: 0,
            launched: Vec::new(),
            sends: Vec::new(),
            lose_next: None,
            refuse_launches: None,
            watching: None,
            shown_at_launch: Vec::new(),
            unreadable: BTreeSet::new(),
        };
        Self { state: Mutex::new(state) }
    }

    pub fn with<R>(&self, change: impl FnOnce(&mut State) -> R) -> R {
        change(&mut self.state.lock().expect("no test panics while holding the fake"))
    }

    async fn answering(&self) -> Result<(), T3Error> {
        match self.with(|state| state.down) {
            None => Ok(()),
            Some(Down::Refused) => Err(T3Error::NoSession(Box::new(T3Error::Unreachable("connection refused".into())))),
            Some(Down::LoggedOut) => Err(T3Error::NoSession(Box::new(T3Error::Unauthorized))),
            Some(Down::Silent) => std::future::pending().await,
        }
    }
}

/// Why `environment` and `projects` fail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Down {
    Refused,
    LoggedOut,
    Silent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Write {
    Launch,
    Send,
    Answer,
}

pub fn opus() -> ModelSelection {
    ModelSelection { provider: ProviderInstanceId::new("claudeAgent"), model: ModelId::new("claude-opus-5-5") }
}

impl State {
    pub fn now(&mut self) -> T3Time {
        self.clock += 1;
        T3Time(self.clock)
    }

    pub fn add(&mut self, id: &str, title: &str, lineage: Lineage) -> ThreadId {
        let at = self.now();
        let thread = ThreadSummary {
            id: ThreadId::new(id),
            title: title.into(),
            status: ThreadStatus::Running,
            lineage,
            runtime_mode: RuntimeMode::FullAccess,
            model: opus(),
            item_count: 0,
            settled: false,
            created_at: at,
            updated_at: at,
        };
        self.threads.push(thread);
        ThreadId::new(id)
    }

    pub fn insert_item(&mut self, thread: &ThreadId, position: usize, id: &str) {
        let at = self.now();
        let item = Item {
            id: ItemId::new(id),
            kind: ItemKind::AssistantMessage,
            status: ItemStatus::Completed,
            run: None,
            title: None,
            text: ItemText { shown: id.into(), more_at: None },
            updated_at: at,
        };
        let items = self.items.entry(thread.clone()).or_default();
        items.insert(position, item);
        let count = items.len() as u64;
        let row = self.thread_mut(thread);
        (row.item_count, row.updated_at) = (count, at);
    }

    fn respond(&mut self, write: Write) -> Result<(), T3Error> {
        match self.lose_next.take_if(|lost| *lost == write) {
            Some(_) => Err(T3Error::Lost(format!("the {write:?} response never arrived"))),
            None => Ok(()),
        }
    }

    pub fn thread_mut(&mut self, id: &ThreadId) -> &mut ThreadSummary {
        self.threads.iter_mut().find(|t| &t.id == id).expect("the thread exists in the fake")
    }

    pub fn item_ids(&self, thread: &ThreadId) -> Vec<ItemId> {
        self.items.get(thread).map_or_else(Vec::new, |items| items.iter().map(|i| i.id.clone()).collect())
    }

    fn by_recency(&self) -> Vec<ThreadSummary> {
        let mut threads = self.threads.clone();
        threads.sort_by_key(|t| (Reverse(t.updated_at), Reverse(t.id.clone())));
        threads
    }
}

impl T3Api for FakeT3 {
    /// A T3 that lists one project, `fake`, and wants a login when there is no token.
    fn connect(endpoint: Endpoint, token: Option<Token>) -> Self {
        let root = "/fake".into();
        let fake = Self::new(Project { id: ProjectId::new("fake"), title: "fake".into(), root, default_model: None });
        fake.with(|state| {
            state.address = endpoint.address();
            state.down = token.is_none().then_some(Down::LoggedOut);
            state.access = token.as_ref().and_then(|token| token.access);
            state.token = token.map(|token| token.secret.0);
        });
        fake
    }

    fn access(&self) -> Option<Access> {
        self.with(|state| state.access)
    }

    /// Any code pairs, and the token names it.
    async fn pair(&self, access: Access, code: PairingCode) -> Result<Credentials, LoginError> {
        let code = match code {
            PairingCode::Mint => "minted".into(),
            PairingCode::Given(code) => code,
        };
        let expires_at = SystemTime::now() + Duration::from_secs(30 * 86_400);
        Ok(Credentials { origin: self.address(), token: format!("token-{code}"), access, expires_at })
    }

    fn use_token(&self, token: Token) {
        self.with(|state| {
            state.down.take_if(|down| *down == Down::LoggedOut);
            state.access = token.access;
            state.token = Some(token.secret.0);
        });
    }

    fn address(&self) -> String {
        self.with(|state| state.address.clone())
    }

    async fn environment(&self) -> Result<Environment, T3Error> {
        self.answering().await?;
        Ok(self.with(|state| state.environment.clone()))
    }

    async fn projects(&self) -> Result<Vec<Project>, T3Error> {
        self.answering().await?;
        Ok(self.with(|state| vec![state.project.clone()]))
    }

    async fn models(&self) -> Result<Vec<ModelSelection>, T3Error> {
        Ok(self.with(|state| state.models.clone()))
    }

    async fn threads(&self, _project: &ProjectId, cursor: Option<u64>) -> Result<ThreadPage, T3Error> {
        self.with(|state| {
            state.list_calls += 1;
            let all = state.by_recency();
            let start = cursor.unwrap_or(0) as usize;
            let (unreadable, threads): (Vec<_>, Vec<_>) =
                all.iter().skip(start).take(PAGE).cloned().partition(|t| state.unreadable.contains(t.id.as_str()));
            let next = (start + PAGE < all.len()).then_some((start + PAGE) as u64);
            let unparsed =
                unreadable.into_iter().map(|t| (t.id.clone(), format!("thread {}", t.id.as_str()))).collect();
            Ok(ThreadPage { threads, next, unparsed })
        })
    }

    async fn threads_titled(&self, _project: &ProjectId, needle: &str) -> Result<Vec<ThreadSummary>, T3Error> {
        Ok(self.with(|state| state.by_recency().into_iter().filter(|t| t.title.contains(needle)).collect()))
    }

    async fn read(&self, thread: &ThreadId, query: ReadQuery) -> Result<TimelinePage, T3Error> {
        self.with(|state| {
            let summary = (state.threads.iter().find(|t| &t.id == thread).cloned())
                .ok_or_else(|| T3Error::Rejected(format!("thread_not_found: {}", thread.as_str())))?;
            let items = state.items.get(thread).cloned().unwrap_or_default();
            let start = query.after.map_or(0, |Position(after)| after as usize + 1);
            let (unreadable, page): (Vec<Item>, Vec<Item>) = (items.iter().skip(start))
                .take(usize::from(query.limit.get()))
                .cloned()
                .partition(|item| state.unreadable.contains(item.id.as_str()));
            Ok(TimelinePage {
                detail: ThreadDetail {
                    summary,
                    branch: None,
                    worktree: None,
                    active_run: None,
                    pending_requests: 0,
                    archived: false,
                },
                earlier: start.min(items.len()) as u64,
                has_more: start + page.len() + unreadable.len() < items.len(),
                items: page,
                unparsed: unreadable.into_iter().map(|i| (i.id.clone(), format!("item {}", i.id.as_str()))).collect(),
            })
        })
    }

    async fn launch(&self, launch: Launch) -> Result<ThreadId, T3Error> {
        self.with(|state| {
            state.launch_calls += 1;
            if let Some(view) = &state.watching {
                let shown = view.borrow().as_ref().map(|view| view.coordinator.clone());
                state.shown_at_launch.push(shown);
            }
            if let Some(reason) = &state.refuse_launches {
                return Err(T3Error::Rejected(reason.clone()));
            }
            let model = (launch.model.or_else(|| state.project.default_model.clone())).ok_or_else(|| {
                T3Error::Rejected("invalid_request: Pass modelSelection: the project has no default model.".into())
            })?;
            let id = format!("mcp:launched-{}", state.launched.len() + 1);
            let thread = state.add(&id, &launch.title, Lineage::Root { via_mcp: true });
            state.thread_mut(&thread).model = model;
            state.insert_item(&thread, 0, &launch.message);
            state.launched.push(thread.clone());
            state.respond(Write::Launch)?;
            Ok(thread)
        })
    }

    async fn send(&self, thread: &ThreadId, text: &str, id: &SendId) -> Result<(), T3Error> {
        self.with(|state| {
            state.sends.push((thread.clone(), text.into(), id.clone()));
            state.respond(Write::Send)
        })
    }

    async fn pending(&self, thread: &ThreadId) -> Result<Vec<RequestId>, T3Error> {
        Ok(self.with(|state| state.questions.iter().filter(|q| &q.thread == thread).map(|q| q.id.clone()).collect()))
    }

    async fn question(&self, _thread: &ThreadId, request: &RequestId) -> Result<PendingRequest, T3Error> {
        self.with(|state| state.questions.iter().find(|q| &q.id == request).cloned())
            .ok_or_else(|| T3Error::Rejected("request_not_found".into()))
    }

    async fn answer(&self, question: &PendingRequest, _answers: &Answers) -> Result<(), T3Error> {
        self.with(|state| {
            state.questions.retain(|q| q.id != question.id);
            state.respond(Write::Answer)
        })
    }
}
