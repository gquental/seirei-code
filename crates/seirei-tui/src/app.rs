use crate::keymap::{self, Action, Context};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use seirei_proto::{
    ids::{ProjectRef, RequestId, SendId, ThreadId},
    model::{
        self, Access, Answer, Answers, CoordinatorView, Job, Machine, MachineProjects, PendingRequest, Project,
        ProjectView, Question, RuntimeMode, ThreadSummary,
    },
    timeline::Timeline,
    wire::{BuildId, CallError, CallId, ClientMsg, Command, Reply, ServerMsg, Subscription},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

const SCROLL_STEP: usize = 3;

#[derive(Debug, PartialEq)]
pub enum Event {
    Connected,
    Disconnected(String),
    Server(ServerMsg),
    Key(KeyEvent),
    Resize,
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    Send(ClientMsg),
    Reconnect,
    Quit,
}

#[derive(Debug, PartialEq)]
pub enum Link {
    Connecting,
    Up,
    Lost(String),
    WrongBuild(BuildId),
}

pub struct App {
    pub link: Link,
    pub screen: Screen,
    pub projects: Vec<MachineProjects>,
    pub help: bool,
    pub notice: Option<String>,
    build: BuildId,
    open: Option<String>,
    take_over: bool,
    calls: HashMap<CallId, Purpose>,
    next_call: u64,
    send_ids: SendIds,
}

enum Purpose {
    ListProjects,
    Send,
    EnsureCoordinator,
    Answer,
}

pub enum Screen {
    Picker { cursor: usize },
    Project(Box<ProjectScreen>),
}

pub struct ProjectScreen {
    pub id: ProjectRef,
    pub view: Option<Arc<ProjectView>>,
    pub timelines: HashMap<ThreadId, Arc<Timeline>>,
    pub focus: Focus,
    pub mode: Mode,
    pub draft: Draft,
    pub selected: Option<ThreadId>,
    pub conversation_back: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Focus {
    Conversation,
    Jobs,
    Job { thread: ThreadId, back: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Browse,
    Compose,
    ConfirmCreate,
    Answer(AnswerForm),
}

pub struct Draft {
    pub text: String,
    pub send_id: SendId,
    sent: Option<String>,
    pub state: DraftState,
}

#[derive(Debug, PartialEq)]
pub enum DraftState {
    Editing,
    Sending(CallId),
    Failed(CallError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnswerForm {
    pub request: RequestId,
    pub question: usize,
    pub row: Row,
    pub toggled: BTreeSet<usize>,
    pub custom: String,
    pub answers: BTreeMap<String, Answer>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Choice(usize),
    Custom,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Shortfall {
    Thread { thread: ThreadId, mode: RuntimeMode, access: Access },
    Launch,
}

struct SendIds {
    seed: SendId,
    minted: u64,
}

impl SendIds {
    fn next(&mut self) -> SendId {
        self.minted += 1;
        SendId::new(format!("{}-{}", self.seed.as_str(), self.minted))
    }
}

impl App {
    pub fn new(open: Option<String>, build: BuildId, send_seed: SendId) -> Self {
        Self {
            link: Link::Connecting,
            screen: Screen::Picker { cursor: 0 },
            projects: vec![],
            help: false,
            notice: None,
            build,
            open,
            take_over: false,
            calls: HashMap::new(),
            next_call: 0,
            send_ids: SendIds { seed: send_seed, minted: 0 },
        }
    }

    /// Every project the picker offers, machine by machine.
    pub fn listed(&self) -> impl Iterator<Item = (&Machine, &Project)> {
        listed(&self.projects)
    }

    pub fn subscription(&self) -> Subscription {
        let Screen::Project(p) = &self.screen else { return Subscription::default() };
        let job = match &p.focus {
            Focus::Job { thread, .. } => Some(thread.clone()),
            Focus::Conversation | Focus::Jobs => None,
        };
        Subscription { project: Some(p.id.clone()), job }
    }

    pub fn context(&self) -> Context {
        match &self.link {
            Link::WrongBuild(_) => Context::WrongBuild,
            _ if self.help => Context::Help,
            _ => self.screen_context(),
        }
    }

    pub fn screen_context(&self) -> Context {
        match &self.screen {
            Screen::Picker { .. } => Context::Picker,
            Screen::Project(p) => match &p.mode {
                Mode::Browse => Context::Browse,
                Mode::Compose => Context::Compose,
                Mode::ConfirmCreate => Context::Confirm,
                Mode::Answer(AnswerForm { row: Row::Custom, .. }) => Context::AnswerText,
                Mode::Answer(_) => Context::Answer,
            },
        }
    }

    pub fn live(&self, action: Action) -> bool {
        match (self.context(), action) {
            (_, Action::Quit | Action::ToggleHelp) => true,
            (Context::WrongBuild, Action::TakeOver) => !self.take_over,
            (Context::Picker, Action::Up | Action::Down | Action::Open) => self.listed().next().is_some(),
            (context, action) => match &self.screen {
                Screen::Project(p) => p.live(context, action, self.link == Link::Up),
                Screen::Picker { .. } => false,
            },
        }
    }

    pub fn update(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Connected => {
                if let Link::Lost(_) = self.link {
                    self.link = Link::Connecting;
                }
                vec![Effect::Send(ClientMsg::Hello { build: self.build.clone(), take_over: self.take_over })]
            }
            Event::Disconnected(reason) => {
                self.lose_link(reason);
                vec![]
            }
            Event::Server(msg) => self.on_server(msg),
            Event::Key(key) => {
                self.notice = None;
                let context = self.context();
                match keymap::lookup(context, key) {
                    Some(action) if self.live(action) => self.act(action),
                    Some(_) => vec![],
                    None if context.takes_text() => {
                        self.edit_text(key);
                        vec![]
                    }
                    None => vec![],
                }
            }
            Event::Resize => vec![],
        }
    }

    pub fn shortfall(&self) -> Option<Shortfall> {
        let Screen::Project(p) = &self.screen else { return None };
        let (context, up) = (self.context(), self.link == Link::Up);
        (keymap::hints(context).into_iter())
            .find_map(|(_, action, _)| p.denied(context, action).filter(|_| p.ready(context, action, up)))
    }

    fn on_server(&mut self, msg: ServerMsg) -> Vec<Effect> {
        match msg {
            ServerMsg::Welcome => {
                self.link = Link::Up;
                self.take_over = false;
                vec![self.call(Purpose::ListProjects, Command::ListProjects)]
            }
            ServerMsg::WrongBuild { daemon } => {
                self.link = Link::WrongBuild(daemon);
                vec![]
            }
            ServerMsg::Yielding => {
                self.link = Link::Lost("the old daemon is exiting so this build can start its own".into());
                vec![]
            }
            ServerMsg::Project(view) => {
                if let Screen::Project(p) = &mut self.screen
                    && (&p.id.machine, &p.id.project) == (&view.machine, &view.project.id)
                    && let Some(notice) = p.accept_view(view)
                {
                    self.notice = Some(notice);
                }
                vec![]
            }
            ServerMsg::Timeline(timeline) => {
                if let Screen::Project(p) = &mut self.screen
                    && wants(&p.focus, p.view.as_deref(), &timeline.thread)
                {
                    p.timelines.insert(timeline.thread.clone(), timeline);
                }
                vec![]
            }
            ServerMsg::Reply { id, result } => self.resolve(id, result),
        }
    }

    fn resolve(&mut self, id: CallId, result: Result<Reply, CallError>) -> Vec<Effect> {
        let Some(purpose) = self.calls.remove(&id) else { return vec![] };
        match (purpose, result) {
            (Purpose::ListProjects, Ok(Reply::Projects(projects))) => {
                self.projects = projects;
                let listed = self.listed().count();
                if let Screen::Picker { cursor } = &mut self.screen {
                    *cursor = (*cursor).min(listed.saturating_sub(1));
                }
                self.open_named()
            }
            (Purpose::ListProjects, Err(e)) => self.notice = Some(format!("Could not list projects: {}", e.message)),
            (Purpose::Send, result) => {
                if let Screen::Project(p) = &mut self.screen
                    && p.draft.state == DraftState::Sending(id)
                {
                    match result {
                        Ok(_) => p.draft = Draft::new(self.send_ids.next()),
                        Err(e) => p.draft.state = DraftState::Failed(e),
                    }
                }
            }
            (Purpose::EnsureCoordinator, Err(e)) => {
                self.notice = Some(format!("Could not create the coordinator: {}", e.message))
            }
            (Purpose::Answer, Ok(_)) => self.notice = Some("Answer sent.".into()),
            (Purpose::Answer, Err(e)) if e.retry_safe => self.notice = Some(format!("Answer not sent: {}", e.message)),
            (Purpose::Answer, Err(e)) => {
                self.notice =
                    Some(format!("{} The answer may have been recorded; check T3 before retrying.", e.message))
            }
            (Purpose::ListProjects | Purpose::EnsureCoordinator, _) => {}
        }
        vec![]
    }

    fn open_named(&mut self) {
        let Some(name) = self.open.take() else { return };
        match model::find_project(&self.projects, &name) {
            Ok(at) => self.enter(at),
            Err(e) => self.notice = Some(format!("Cannot open {name}: {e}.")),
        }
    }

    fn enter(&mut self, at: ProjectRef) {
        self.screen = Screen::Project(Box::new(ProjectScreen::new(at, self.send_ids.next())));
    }

    fn act(&mut self, action: Action) -> Vec<Effect> {
        match (self.context(), action) {
            (_, Action::Quit) => vec![Effect::Quit],
            (_, Action::ToggleHelp) => {
                self.help = !self.help;
                vec![]
            }
            (_, Action::TakeOver) => {
                self.take_over = true;
                self.link = Link::Connecting;
                vec![Effect::Reconnect]
            }
            (Context::Picker, action) => {
                let Screen::Picker { cursor } = &mut self.screen else { return vec![] };
                match action {
                    Action::Up => *cursor = cursor.saturating_sub(1),
                    Action::Down => *cursor = (*cursor + 1).min(listed(&self.projects).count().saturating_sub(1)),
                    _ => {
                        let index = *cursor;
                        let picked = self.listed().nth(index).map(|(machine, project)| ProjectRef {
                            machine: machine.id.clone(),
                            project: project.id.clone(),
                        });
                        if let Some(at) = picked {
                            self.enter(at);
                        }
                    }
                }
                vec![]
            }
            (Context::Compose, Action::Submit) => self.submit_draft(),
            (Context::Answer | Context::AnswerText, Action::Submit) => self.submit_answer(),
            (Context::Confirm, Action::Confirm) => {
                let Screen::Project(p) = &mut self.screen else { return vec![] };
                p.mode = Mode::Browse;
                let project = p.id.clone();
                vec![self.call(Purpose::EnsureCoordinator, Command::EnsureCoordinator { project })]
            }
            (Context::Browse, Action::Back) => {
                let Screen::Project(p) = &mut self.screen else { return vec![] };
                if let Focus::Job { .. } = p.focus {
                    p.set_focus(Focus::Jobs);
                    return vec![];
                }
                let cursor = listed(&self.projects)
                    .position(|(machine, project)| machine.id == p.id.machine && project.id == p.id.project)
                    .unwrap_or(0);
                self.screen = Screen::Picker { cursor };
                vec![self.call(Purpose::ListProjects, Command::ListProjects)]
            }
            (context, action) => {
                if let Screen::Project(p) = &mut self.screen {
                    p.act(context, action);
                }
                vec![]
            }
        }
    }

    fn submit_draft(&mut self) -> Vec<Effect> {
        let id = self.next_call_id();
        let Screen::Project(p) = &mut self.screen else { return vec![] };
        let Some(coordinator) = p.coordinator().map(|c| c.id.clone()) else { return vec![] };
        if p.draft.sent.as_ref().is_some_and(|sent| *sent != p.draft.text) {
            p.draft.send_id = self.send_ids.next();
        }
        p.draft.sent = Some(p.draft.text.clone());
        let command = Command::Send {
            project: p.id.clone(),
            thread: coordinator,
            text: p.draft.text.clone(),
            send_id: p.draft.send_id.clone(),
        };
        p.draft.state = DraftState::Sending(id);
        self.calls.insert(id, Purpose::Send);
        vec![Effect::Send(ClientMsg::Call { id, command })]
    }

    fn submit_answer(&mut self) -> Vec<Effect> {
        let Screen::Project(p) = &mut self.screen else { return vec![] };
        let Some((request, question)) = p.answering().map(|(_, r, q)| (r.clone(), q.clone())) else {
            return vec![];
        };
        let Mode::Answer(form) = &mut p.mode else { return vec![] };
        let Some(answer) = form.answer(&question) else { return vec![] };
        form.answers.insert(question.id, answer);
        if let Some(next) = request.questions.get(form.question + 1) {
            *form = AnswerForm {
                question: form.question + 1,
                answers: std::mem::take(&mut form.answers),
                ..AnswerForm::new(request.id, next)
            };
            return vec![];
        }
        let answers = Answers(std::mem::take(&mut form.answers));
        p.mode = Mode::Browse;
        let command = Command::Answer { project: p.id.clone(), request: request.id, answers };
        vec![self.call(Purpose::Answer, command)]
    }

    fn lose_link(&mut self, reason: String) {
        if !matches!(self.link, Link::WrongBuild(_)) {
            self.link = Link::Lost(reason);
        }
        if let Screen::Project(p) = &mut self.screen
            && let DraftState::Sending(_) = p.draft.state
        {
            let message = "Connection lost before the daemon confirmed. Enter resends; it cannot duplicate.".into();
            p.draft.state = DraftState::Failed(CallError { message, retry_safe: true });
        }
        if self.calls.values().any(|p| matches!(p, Purpose::Answer)) {
            self.notice =
                Some("Lost the daemon before it confirmed your answer. Check T3 before answering again.".into());
        }
        self.calls.clear();
    }

    fn edit_text(&mut self, key: KeyEvent) {
        let typed = match key.code {
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => Some(c),
            KeyCode::Backspace => None,
            _ => return,
        };
        let Screen::Project(p) = &mut self.screen else { return };
        let text = match &mut p.mode {
            Mode::Compose if matches!(p.draft.state, DraftState::Sending(_)) => return,
            Mode::Compose => {
                p.draft.state = DraftState::Editing;
                &mut p.draft.text
            }
            Mode::Answer(form) => &mut form.custom,
            Mode::Browse | Mode::ConfirmCreate => return,
        };
        match typed {
            Some(c) => text.push(c),
            None => drop(text.pop()),
        }
    }

    fn call(&mut self, purpose: Purpose, command: Command) -> Effect {
        let id = self.next_call_id();
        self.calls.insert(id, purpose);
        Effect::Send(ClientMsg::Call { id, command })
    }

    fn next_call_id(&mut self) -> CallId {
        self.next_call += 1;
        CallId(self.next_call)
    }
}

impl Draft {
    fn new(send_id: SendId) -> Self {
        Self { text: String::new(), send_id, sent: None, state: DraftState::Editing }
    }
}

impl ProjectScreen {
    fn new(id: ProjectRef, send_id: SendId) -> Self {
        Self {
            id,
            view: None,
            timelines: HashMap::new(),
            focus: Focus::Conversation,
            mode: Mode::Browse,
            draft: Draft::new(send_id),
            selected: None,
            conversation_back: 0,
        }
    }

    pub fn coordinator(&self) -> Option<&ThreadSummary> {
        match &self.view.as_deref()?.coordinator {
            CoordinatorView::Ready(thread) => Some(thread),
            CoordinatorView::Absent | CoordinatorView::Starting => None,
        }
    }

    pub fn jobs(&self) -> &[Job] {
        self.view.as_deref().map_or(&[], |v| &v.jobs)
    }

    pub fn questions(&self) -> &[PendingRequest] {
        self.view.as_deref().map_or(&[], |v| &v.questions)
    }

    fn thread(&self, id: &ThreadId) -> Option<&ThreadSummary> {
        let jobs = self.jobs().iter().flat_map(|job| std::iter::once(&job.thread).chain(&job.subtasks));
        self.coordinator().into_iter().chain(jobs).find(|thread| &thread.id == id)
    }

    pub fn selected_job(&self) -> Option<&Job> {
        let selected = self.selected.as_ref()?;
        self.jobs().iter().find(|job| &job.thread.id == selected)
    }

    pub fn answering(&self) -> Option<(&AnswerForm, &PendingRequest, &Question)> {
        let Mode::Answer(form) = &self.mode else { return None };
        let request = self.questions().iter().find(|r| r.id == form.request)?;
        Some((form, request, request.questions.get(form.question)?))
    }

    fn live(&self, context: Context, action: Action, up: bool) -> bool {
        self.ready(context, action, up) && self.denied(context, action).is_none()
    }

    fn ready(&self, context: Context, action: Action, up: bool) -> bool {
        let sending = matches!(self.draft.state, DraftState::Sending(_));
        match (context, action) {
            (Context::Browse, Action::Back) => true,
            (Context::Browse, Action::Up | Action::Down) => self.focus != Focus::Jobs || !self.jobs().is_empty(),
            (Context::Browse, Action::Open) => self.focus == Focus::Jobs && self.selected_job().is_some(),
            (Context::Browse, Action::NextPane) => !matches!(self.focus, Focus::Job { .. }),
            (Context::Browse, Action::Compose) => self.coordinator().is_some(),
            (Context::Browse, Action::CreateCoordinator) => {
                self.view.as_deref().is_some_and(|v| v.coordinator == CoordinatorView::Absent)
            }
            (Context::Browse, Action::AnswerQuestion) => !self.questions().is_empty(),
            (Context::Compose, Action::Submit) => {
                up && !sending && self.coordinator().is_some() && !self.draft.text.trim().is_empty()
            }
            (Context::Compose, Action::Newline) => !sending,
            (Context::Compose | Context::Confirm, Action::Back) => true,
            (Context::Confirm, Action::Confirm) => up,
            (Context::Answer | Context::AnswerText, Action::Up | Action::Down | Action::Back) => true,
            (Context::Answer, Action::Toggle) => {
                self.answering().is_some_and(|(form, _, q)| q.multi_select && matches!(form.row, Row::Choice(_)))
            }
            (Context::Answer | Context::AnswerText, Action::Submit) => self
                .answering()
                .is_some_and(|(form, r, q)| form.answer(q).is_some() && (up || form.question + 1 < r.questions.len())),
            _ => false,
        }
    }

    fn denied(&self, context: Context, action: Action) -> Option<Shortfall> {
        let access = self.view.as_deref()?.access?;
        let target = match (context, action) {
            (Context::Browse, Action::Compose) | (Context::Compose, Action::Submit) => &self.coordinator()?.id,
            (Context::Browse, Action::AnswerQuestion) => &self.questions().first()?.thread,
            (Context::Answer | Context::AnswerText, Action::Submit) => &self.answering()?.1.thread,
            (Context::Browse, Action::CreateCoordinator) | (Context::Confirm, Action::Confirm) => {
                return (access == Access::ReadOnly).then_some(Shortfall::Launch);
            }
            _ => return None,
        };
        let mode = self.thread(target)?.runtime_mode;
        (!access.allows(mode)).then(|| Shortfall::Thread { thread: target.clone(), mode, access })
    }

    fn act(&mut self, context: Context, action: Action) {
        match (context, action) {
            (Context::Browse, Action::Up | Action::Down) => {
                let older = action == Action::Up;
                match &mut self.focus {
                    Focus::Conversation => scroll(&mut self.conversation_back, older),
                    Focus::Job { back, .. } => scroll(back, older),
                    Focus::Jobs => self.move_selection(if older { -1 } else { 1 }),
                }
            }
            (Context::Browse, Action::Open) => {
                if let Some(thread) = self.selected.clone() {
                    self.set_focus(Focus::Job { thread, back: 0 });
                }
            }
            (Context::Browse, Action::NextPane) => match self.focus {
                Focus::Conversation => {
                    if self.selected_job().is_none() {
                        self.selected = self.jobs().first().map(|job| job.thread.id.clone());
                    }
                    self.focus = Focus::Jobs;
                }
                Focus::Jobs | Focus::Job { .. } => self.set_focus(Focus::Conversation),
            },
            (Context::Browse, Action::Compose) => self.mode = Mode::Compose,
            (Context::Browse, Action::CreateCoordinator) => self.mode = Mode::ConfirmCreate,
            (Context::Browse, Action::AnswerQuestion) => {
                if let Some(request) = self.questions().first()
                    && let Some(first) = request.questions.first()
                {
                    self.mode = Mode::Answer(AnswerForm::new(request.id.clone(), first));
                }
            }
            (Context::Compose, Action::Newline) => {
                self.draft.text.push('\n');
                self.draft.state = DraftState::Editing;
            }
            (Context::Compose | Context::Confirm | Context::Answer | Context::AnswerText, Action::Back) => {
                self.mode = Mode::Browse
            }
            (Context::Answer | Context::AnswerText, Action::Up | Action::Down) => {
                let Some(rows) = self.answering().map(|(_, _, q)| AnswerForm::rows(q)) else { return };
                if let Mode::Answer(form) = &mut self.mode {
                    let at = rows.iter().position(|row| *row == form.row).unwrap_or(0);
                    let to = if action == Action::Up { at.saturating_sub(1) } else { (at + 1).min(rows.len() - 1) };
                    form.row = rows[to];
                }
            }
            (Context::Answer, Action::Toggle) => {
                if let Mode::Answer(AnswerForm { row: Row::Choice(i), toggled, .. }) = &mut self.mode
                    && !toggled.remove(i)
                {
                    toggled.insert(*i);
                }
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let jobs = self.jobs();
        let Some(last) = jobs.len().checked_sub(1) else { return };
        let at = self.selected.as_ref().and_then(|id| jobs.iter().position(|job| &job.thread.id == id));
        let to = match at {
            Some(at) => at.saturating_add_signed(delta).min(last),
            None => 0,
        };
        self.selected = Some(jobs[to].thread.id.clone());
    }

    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.prune();
    }

    fn prune(&mut self) {
        let (focus, view) = (&self.focus, self.view.as_deref());
        self.timelines.retain(|thread, _| wants(focus, view, thread));
    }

    fn accept_view(&mut self, view: Arc<ProjectView>) -> Option<String> {
        self.view = Some(view);
        self.prune();
        match &self.mode {
            Mode::Answer(form) if !self.questions().iter().any(|r| r.id == form.request) => {
                self.mode = Mode::Browse;
                Some("That question was answered elsewhere.".into())
            }
            Mode::ConfirmCreate if self.view.as_deref().is_some_and(|v| v.coordinator != CoordinatorView::Absent) => {
                self.mode = Mode::Browse;
                None
            }
            _ => None,
        }
    }
}

impl AnswerForm {
    fn new(request: RequestId, question: &Question) -> Self {
        Self {
            request,
            question: 0,
            row: Self::rows(question)[0],
            toggled: BTreeSet::new(),
            custom: String::new(),
            answers: BTreeMap::new(),
        }
    }

    /// Even a question with no options and no custom answers gets the free-text row: T3 needs some answer.
    pub fn rows(question: &Question) -> Vec<Row> {
        let choices = (0..question.options.len()).map(Row::Choice);
        let rows: Vec<Row> = choices.chain(question.allow_custom.then_some(Row::Custom)).collect();
        if rows.is_empty() { vec![Row::Custom] } else { rows }
    }

    pub fn answer(&self, question: &Question) -> Option<Answer> {
        match self.row {
            Row::Custom => {
                let text = self.custom.trim();
                (!text.is_empty()).then(|| Answer::Custom(text.into()))
            }
            Row::Choice(_) if question.multi_select && !self.toggled.is_empty() => {
                Some(Answer::Picked(self.toggled.iter().copied().collect()))
            }
            Row::Choice(i) => (i < question.options.len()).then(|| Answer::Picked(vec![i])),
        }
    }
}

fn listed(projects: &[MachineProjects]) -> impl Iterator<Item = (&Machine, &Project)> {
    (projects.iter()).flat_map(|listed| listed.projects.iter().map(move |project| (&listed.machine, project)))
}

fn wants(focus: &Focus, view: Option<&ProjectView>, thread: &ThreadId) -> bool {
    let open_job = matches!(focus, Focus::Job { thread: open, .. } if open == thread);
    let coordinator = match view.map(|v| &v.coordinator) {
        None => true,
        Some(CoordinatorView::Ready(c)) => &c.id == thread,
        Some(CoordinatorView::Absent | CoordinatorView::Starting) => false,
    };
    open_job || coordinator
}

fn scroll(back: &mut usize, older: bool) {
    *back = if older { back.saturating_add(SCROLL_STEP) } else { back.saturating_sub(SCROLL_STEP) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use crossterm::event::KeyCode::{Backspace, Char, Enter, Esc, Tab};
    use seirei_proto::{
        ids::{MachineId, ProjectId},
        model::MachineStatus,
    };

    fn at(machine: &str, project: &str) -> ProjectRef {
        ProjectRef { machine: MachineId::parse(machine).unwrap(), project: ProjectId::new(project) }
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, Char(c));
        }
    }

    fn calls(effects: &[Effect]) -> Vec<(CallId, &Command)> {
        (effects.iter())
            .filter_map(|e| match e {
                Effect::Send(ClientMsg::Call { id, command }) => Some((*id, command)),
                _ => None,
            })
            .collect()
    }

    fn reply(id: CallId, result: Result<Reply, CallError>) -> Event {
        Event::Server(ServerMsg::Reply { id, result })
    }

    fn project(app: &App) -> &ProjectScreen {
        match &app.screen {
            Screen::Project(p) => p,
            Screen::Picker { .. } => panic!("expected a project screen"),
        }
    }

    fn sent_id(effects: &[Effect]) -> SendId {
        match calls(effects).as_slice() {
            [(_, Command::Send { send_id, .. })] => send_id.clone(),
            other => panic!("expected one Send call, got {other:?}"),
        }
    }

    #[test]
    fn welcome_lists_projects_and_open_enters_the_project_and_subscribes() {
        let mut app = new_app(None);
        let hello = app.update(Event::Connected);
        let build = BuildId("test-build".into());
        assert_eq!(
            hello,
            [Effect::Send(ClientMsg::Hello { build, take_over: false })],
            "the build the runtime passed in"
        );
        let effects = app.update(Event::Server(ServerMsg::Welcome));
        assert_eq!(app.link, Link::Up);
        let [(id, Command::ListProjects)] = calls(&effects)[..] else {
            panic!("expected ListProjects, got {effects:?}")
        };
        assert_eq!(app.subscription(), Subscription::default());
        app.update(reply(id, Ok(Reply::Projects(local(vec![project_meta("a", "alpha"), project_meta("b", "beta")])))));

        press(&mut app, Char('j'));
        press(&mut app, Enter);
        assert_eq!(app.subscription(), Subscription { project: Some(at("local", "b")), job: None });
        assert_eq!(app.context(), Context::Browse);
    }

    #[test]
    fn the_project_named_on_the_command_line_opens_once() {
        let mut app = new_app(Some("beta"));
        let effects = app.update(Event::Server(ServerMsg::Welcome));
        let [(id, _)] = calls(&effects)[..] else { panic!() };
        app.update(reply(id, Ok(Reply::Projects(local(vec![project_meta("a", "alpha"), project_meta("b", "beta")])))));
        assert_eq!(project(&app).id, at("local", "b"));

        let effects = press(&mut app, Esc);
        assert!(matches!(app.screen, Screen::Picker { cursor: 1 }));
        let [(id, Command::ListProjects)] = calls(&effects)[..] else { panic!() };
        app.update(reply(id, Ok(Reply::Projects(local(vec![project_meta("a", "alpha"), project_meta("b", "beta")])))));
        assert!(matches!(app.screen, Screen::Picker { .. }));
    }

    #[test]
    fn a_name_on_two_machines_opens_only_when_qualified() {
        let listing = || {
            vec![
                machine("local", MachineStatus::Connected, vec![project_meta("a", "seirei-code")]),
                machine("studio", MachineStatus::Connected, vec![project_meta("b", "seirei-code")]),
            ]
        };
        let opened = |name: &str| {
            let mut app = new_app(Some(name));
            let effects = app.update(Event::Server(ServerMsg::Welcome));
            let [(id, _)] = calls(&effects)[..] else { panic!() };
            app.update(reply(id, Ok(Reply::Projects(listing()))));
            app
        };
        let ambiguous = opened("seirei-code");
        assert!(matches!(ambiguous.screen, Screen::Picker { .. }));
        let notice = ambiguous.notice.as_deref().unwrap_or_default();
        assert!(notice.contains("local/seirei-code, studio/seirei-code"), "{notice}");
        assert_eq!(project(&opened("studio/seirei-code")).id, at("studio", "b"));
    }

    #[test]
    fn compose_is_dead_until_the_coordinator_is_ready() {
        let mut app = app_in_project(view(CoordinatorView::Absent, vec![], vec![]));
        assert!(!app.live(Action::Compose));
        assert!(app.live(Action::CreateCoordinator));
        press(&mut app, Char('i'));
        assert_eq!(app.context(), Context::Browse);

        app.update(Event::Server(ServerMsg::Project(Arc::new(view(CoordinatorView::Starting, vec![], vec![])))));
        assert!(!app.live(Action::Compose));
        assert!(!app.live(Action::CreateCoordinator));

        app.update(Event::Server(ServerMsg::Project(Arc::new(ready_view(vec![], vec![])))));
        press(&mut app, Char('i'));
        assert_eq!(app.context(), Context::Compose);
    }

    fn granted(mut view: ProjectView, access: Access) -> ProjectView {
        view.access = Some(access);
        view
    }

    #[test]
    fn the_grant_kills_messaging_a_coordinator_above_it() {
        let mut app = app_in_project(granted(ready_view(vec![], vec![]), Access::Auto));
        assert!(!app.live(Action::Compose), "the coordinator runs at full-access");
        let expected = Shortfall::Thread {
            thread: ThreadId::new(COORDINATOR),
            mode: RuntimeMode::FullAccess,
            access: Access::Auto,
        };
        assert_eq!(app.shortfall(), Some(expected));
        press(&mut app, Char('i'));
        assert_eq!(app.context(), Context::Browse);

        app.update(Event::Server(ServerMsg::Project(Arc::new(granted(
            ready_view(vec![], vec![]),
            Access::FullAccess,
        )))));
        assert!(app.live(Action::Compose));
        assert_eq!(app.shortfall(), None);
        app.update(Event::Server(ServerMsg::Project(Arc::new(ready_view(vec![], vec![])))));
        assert!(app.live(Action::Compose), "an unknown grant leaves the decision to T3");
    }

    #[test]
    fn the_grant_kills_answering_a_thread_above_it() {
        let request = PendingRequest {
            id: RequestId::new("req"),
            thread: ThreadId::new("j1"),
            questions: vec![question("db", &["Postgres"], false, false)],
        };
        let mut view = granted(ready_view(vec![job("j1", "Fix the parser [job:K]")], vec![request]), Access::Auto);
        if let CoordinatorView::Ready(coordinator) = &mut view.coordinator {
            coordinator.runtime_mode = RuntimeMode::Auto;
        }
        let mut app = app_in_project(view);
        assert!(app.live(Action::Compose), "the coordinator runs within the grant");
        assert!(!app.live(Action::AnswerQuestion), "the asking job runs at full-access");
        let expected =
            Shortfall::Thread { thread: ThreadId::new("j1"), mode: RuntimeMode::FullAccess, access: Access::Auto };
        assert_eq!(app.shortfall(), Some(expected));
        press(&mut app, Char('a'));
        assert_eq!(app.context(), Context::Browse);
    }

    #[test]
    fn only_a_read_only_grant_kills_creating_the_coordinator() {
        let app = app_in_project(granted(view(CoordinatorView::Absent, vec![], vec![]), Access::ReadOnly));
        assert!(!app.live(Action::CreateCoordinator));
        assert_eq!(app.shortfall(), Some(Shortfall::Launch));

        let app = app_in_project(granted(view(CoordinatorView::Absent, vec![], vec![]), Access::ApprovalRequired));
        assert!(app.live(Action::CreateCoordinator), "the coordinator launches at whatever ceiling the login has");
        assert_eq!(app.shortfall(), None);

        let app = app_in_project(granted(view(CoordinatorView::Starting, vec![], vec![]), Access::ReadOnly));
        assert_eq!(app.shortfall(), None, "nothing to create, so the grant is not what keeps it dead");
    }

    #[test]
    fn create_coordinator_confirms_before_calling() {
        let mut app = app_in_project(view(CoordinatorView::Absent, vec![], vec![]));
        assert!(press(&mut app, Char('c')).is_empty());
        assert_eq!(app.context(), Context::Confirm);
        let effects = press(&mut app, Char('y'));
        assert!(matches!(calls(&effects)[..], [(_, Command::EnsureCoordinator { .. })]));
        assert_eq!(app.context(), Context::Browse);
    }

    #[test]
    fn a_confirmed_send_clears_the_draft_and_mints_a_new_key() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Char('i'));
        assert!(!app.live(Action::Submit), "an empty draft has nothing to send");
        type_text(&mut app, "add a thumbnail view");
        let effects = press(&mut app, Enter);
        assert_eq!(sent_id(&effects), SendId::new("seed-1"), "keys count up from the seed the runtime passed in");
        let [(id, _)] = calls(&effects)[..] else { panic!() };
        type_text(&mut app, "ignored while sending");
        assert_eq!(project(&app).draft.text, "add a thumbnail view");

        app.update(reply(id, Ok(Reply::Done)));
        let draft = &project(&app).draft;
        assert_eq!((draft.text.as_str(), &draft.state), ("", &DraftState::Editing));
        assert_eq!(draft.send_id, SendId::new("seed-2"));
    }

    #[test]
    fn a_refused_send_keeps_its_text_and_key() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Char('i'));
        type_text(&mut app, "hi");
        let effects = press(&mut app, Enter);
        let first = sent_id(&effects);
        let [(id, _)] = calls(&effects)[..] else { panic!() };
        let error = CallError { message: "T3 is unreachable".into(), retry_safe: true };
        app.update(reply(id, Err(error.clone())));
        assert_eq!(project(&app).draft.state, DraftState::Failed(error));

        press(&mut app, Backspace);
        let draft = &project(&app).draft;
        assert_eq!((draft.text.as_str(), &draft.state, &draft.send_id), ("h", &DraftState::Editing, &first));
    }

    #[test]
    fn resending_a_failed_draft_unchanged_reuses_its_key() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Char('i'));
        type_text(&mut app, "hi");
        let effects = press(&mut app, Enter);
        let [(id, _)] = calls(&effects)[..] else { panic!() };
        app.update(reply(id, Err(CallError { message: "lost contact with T3".into(), retry_safe: false })));
        type_text(&mut app, "!");
        press(&mut app, Backspace);
        assert_eq!(sent_id(&press(&mut app, Enter)), sent_id(&effects), "T3 drops it if the first send landed");
    }

    #[test]
    fn editing_a_failed_draft_makes_it_a_new_message() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Char('i'));
        type_text(&mut app, "hi");
        let effects = press(&mut app, Enter);
        let [(id, _)] = calls(&effects)[..] else { panic!() };
        app.update(reply(id, Err(CallError { message: "lost contact with T3".into(), retry_safe: false })));
        type_text(&mut app, " there");
        let resent = press(&mut app, Enter);
        assert_ne!(sent_id(&resent), sent_id(&effects), "T3 would drop the edit as a duplicate of the first send");
        let [(_, Command::Send { text, .. })] = calls(&resent)[..] else { panic!() };
        assert_eq!(text, "hi there");
    }

    #[test]
    fn a_send_lost_with_the_daemon_resends_under_the_same_key() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Char('i'));
        type_text(&mut app, "add a thumbnail view");
        let first = sent_id(&press(&mut app, Enter));
        assert!(app.update(Event::Disconnected("daemon exited".into())).is_empty());
        assert!(matches!(project(&app).draft.state, DraftState::Failed(CallError { retry_safe: true, .. })));
        assert!(!app.live(Action::Submit), "nothing can be sent while the link is down");

        app.update(Event::Connected);
        app.update(Event::Server(ServerMsg::Welcome));
        assert_eq!(sent_id(&press(&mut app, Enter)), first);
        assert_eq!(app.subscription(), Subscription { project: Some(at("local", "p")), job: None });
    }

    #[test]
    fn alt_enter_adds_a_line_and_esc_keeps_the_draft() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Char('i'));
        type_text(&mut app, "one");
        app.update(Event::Key(KeyEvent::new(Enter, KeyModifiers::ALT)));
        type_text(&mut app, "two");
        press(&mut app, Esc);
        assert_eq!(app.context(), Context::Browse);
        assert_eq!(project(&app).draft.text, "one\ntwo");
    }

    #[test]
    fn frames_for_another_project_or_thread_are_dropped() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        let mut other = ready_view(vec![], vec![]);
        other.project.id = ProjectId::new("elsewhere");
        other.coordinator = CoordinatorView::Absent;
        app.update(Event::Server(ServerMsg::Project(Arc::new(other))));
        let mut same_id_elsewhere = ready_view(vec![], vec![]);
        same_id_elsewhere.machine = MachineId::parse("studio").unwrap();
        same_id_elsewhere.coordinator = CoordinatorView::Absent;
        app.update(Event::Server(ServerMsg::Project(Arc::new(same_id_elsewhere))));
        assert!(project(&app).coordinator().is_some(), "a frame matches by machine and project");

        app.update(Event::Server(ServerMsg::Timeline(Arc::new(timeline("mcp:stranger", vec![])))));
        app.update(Event::Server(ServerMsg::Timeline(Arc::new(timeline(COORDINATOR, vec![])))));
        let kept: Vec<_> = project(&app).timelines.keys().map(ThreadId::as_str).collect();
        assert_eq!(kept, [COORDINATOR]);
    }

    #[test]
    fn the_selection_follows_its_job_when_the_list_reorders() {
        let jobs = vec![job("j1", "First [job:K]"), job("j2", "Second [job:K]"), job("j3", "Third [job:K]")];
        let mut app = app_in_project(ready_view(jobs.clone(), vec![]));
        press(&mut app, Tab);
        press(&mut app, Char('j'));
        assert_eq!(project(&app).selected_job().unwrap().thread.id.as_str(), "j2");

        let reordered = vec![jobs[1].clone(), jobs[2].clone(), jobs[0].clone()];
        app.update(Event::Server(ServerMsg::Project(Arc::new(ready_view(reordered, vec![])))));
        assert_eq!(project(&app).selected_job().unwrap().thread.id.as_str(), "j2");
        press(&mut app, Char('j'));
        assert_eq!(project(&app).selected_job().unwrap().thread.id.as_str(), "j3");

        press(&mut app, Enter);
        let open = Some(ThreadId::new("j3"));
        assert_eq!(app.subscription().job, open);
        press(&mut app, Esc);
        assert_eq!((app.subscription().job, &project(&app).focus), (None, &Focus::Jobs));
    }

    #[test]
    fn open_needs_a_selected_job() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Tab);
        assert!(!app.live(Action::Open));
        assert!(press(&mut app, Enter).is_empty());
        assert_eq!(project(&app).focus, Focus::Jobs);
    }

    #[test]
    fn the_answer_form_collects_picks_and_custom_text() {
        let request = PendingRequest {
            id: RequestId::new("req"),
            thread: ThreadId::new(COORDINATOR),
            questions: vec![
                question("db", &["Postgres", "SQLite", "DuckDB"], true, false),
                question("name", &["thumbs"], false, true),
            ],
        };
        let mut app = app_in_project(ready_view(vec![], vec![request]));
        assert!(app.live(Action::AnswerQuestion));
        press(&mut app, Char('a'));
        assert_eq!(app.context(), Context::Answer);
        press(&mut app, Char(' '));
        press(&mut app, Char('j'));
        press(&mut app, Char('j'));
        press(&mut app, Char(' '));
        assert!(press(&mut app, Enter).is_empty());

        assert!(!app.live(Action::Toggle), "the second question is single-select");
        press(&mut app, Char('j'));
        assert_eq!(app.context(), Context::AnswerText);
        assert!(!app.live(Action::Submit), "blank free text is no answer");
        type_text(&mut app, "jk thumbnails ");
        let effects = press(&mut app, Enter);

        let expected = Answers(BTreeMap::from([
            ("db".to_string(), Answer::Picked(vec![0, 2])),
            ("name".to_string(), Answer::Custom("jk thumbnails".into())),
        ]));
        match calls(&effects)[..] {
            [(_, Command::Answer { request, answers, .. })] => {
                assert_eq!((request.as_str(), answers), ("req", &expected))
            }
            _ => panic!("expected one Answer call, got {effects:?}"),
        }
        assert_eq!(app.context(), Context::Browse);
    }

    #[test]
    fn an_answer_form_closes_when_its_question_disappears() {
        let request = PendingRequest {
            id: RequestId::new("req"),
            thread: ThreadId::new(COORDINATOR),
            questions: vec![question("db", &["Postgres"], false, false)],
        };
        let mut app = app_in_project(ready_view(vec![], vec![request]));
        press(&mut app, Char('a'));
        app.update(Event::Server(ServerMsg::Project(Arc::new(ready_view(vec![], vec![])))));
        assert_eq!(app.context(), Context::Browse);
        assert!(app.notice.is_some());
    }

    #[test]
    fn take_over_rides_every_hello_until_welcomed() {
        let mut app = new_app(None);
        app.update(Event::Connected);
        app.update(Event::Server(ServerMsg::WrongBuild { daemon: BuildId("old".into()) }));
        assert_eq!(app.context(), Context::WrongBuild);
        app.update(Event::Disconnected("closed".into()));
        assert!(matches!(app.link, Link::WrongBuild(_)), "the prompt survives the daemon hanging up");

        assert_eq!(press(&mut app, Char('r')), vec![Effect::Reconnect]);
        assert!(!app.live(Action::TakeOver));
        for _ in 0..2 {
            let hello = app.update(Event::Connected);
            assert!(matches!(hello.as_slice(), [Effect::Send(ClientMsg::Hello { take_over: true, .. })]));
            app.update(Event::Server(ServerMsg::Yielding));
            app.update(Event::Disconnected("closed".into()));
        }
        app.update(Event::Connected);
        app.update(Event::Server(ServerMsg::Welcome));
        let hello = app.update(Event::Connected);
        assert!(matches!(hello.as_slice(), [Effect::Send(ClientMsg::Hello { take_over: false, .. })]));
    }

    #[test]
    fn scrolling_back_and_forward_returns_to_following() {
        let mut app = app_in_project(ready_view(vec![], vec![]));
        press(&mut app, Char('k'));
        press(&mut app, Char('k'));
        assert_eq!(project(&app).conversation_back, 2 * SCROLL_STEP);
        for _ in 0..3 {
            press(&mut app, Char('j'));
        }
        assert_eq!(project(&app).conversation_back, 0);
    }
}
