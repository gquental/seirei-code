use crate::app::{App, Event, Link};
use seirei_proto::{
    ids::{MachineId, ModelId, ProjectId, ProviderInstanceId, SendId, T3Time, ThreadId},
    model::{
        Choice, CoordinatorView, Health, Job, JobLink, Lineage, Machine, MachineProjects, MachineStatus,
        ModelSelection, PendingRequest, Project, ProjectView, Question, RuntimeMode, ThreadStatus, ThreadSummary,
    },
    timeline::{Item, Timeline},
    wire::{BuildId, CallId, Reply, ServerMsg},
};
use std::sync::Arc;

pub const COORDINATOR: &str = "mcp:coord";

pub fn project_meta(id: &str, title: &str) -> Project {
    Project { id: ProjectId::new(id), title: title.into(), root: format!("/src/{title}").into(), default_model: None }
}

pub fn machine(name: &str, status: MachineStatus, projects: Vec<Project>) -> MachineProjects {
    let id = MachineId::parse(name).expect("a machine name");
    let address = format!("https://{name}.tail123b88.ts.net");
    MachineProjects { machine: Machine { id, address, label: None, status }, projects }
}

pub fn local(projects: Vec<Project>) -> Vec<MachineProjects> {
    vec![machine("local", MachineStatus::Connected, projects)]
}

pub fn model(provider: &str, model: &str) -> ModelSelection {
    ModelSelection { provider: ProviderInstanceId::new(provider), model: ModelId::new(model) }
}

pub fn thread(id: &str, title: &str, status: ThreadStatus) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId::new(id),
        title: title.into(),
        status,
        lineage: Lineage::Root { via_mcp: true },
        runtime_mode: RuntimeMode::FullAccess,
        model: model("claudeAgent", "claude-opus-5-5"),
        item_count: 1,
        settled: false,
        created_at: T3Time(0),
        updated_at: T3Time(0),
    }
}

pub fn job(id: &str, title: &str) -> Job {
    let thread = ThreadSummary { model: model("codex", "gpt-6.1-sol"), ..thread(id, title, ThreadStatus::Running) };
    Job { thread, link: JobLink::Launched, subtasks: vec![] }
}

pub fn view(coordinator: CoordinatorView, jobs: Vec<Job>, questions: Vec<PendingRequest>) -> ProjectView {
    ProjectView {
        machine: MachineId::local(),
        project: project_meta("p", "seirei-code"),
        coordinator,
        jobs,
        questions,
        health: Health { last_sync: None, problem: None },
        access: None,
    }
}

pub fn ready_view(jobs: Vec<Job>, questions: Vec<PendingRequest>) -> ProjectView {
    let coordinator = thread(COORDINATOR, "Coordinator seirei:K", ThreadStatus::Idle);
    view(CoordinatorView::Ready(coordinator), jobs, questions)
}

pub fn question(id: &str, options: &[&str], multi_select: bool, allow_custom: bool) -> Question {
    let choice = |label: &&str| Choice { label: label.to_string(), description: format!("about {label}"), value: None };
    Question {
        id: id.into(),
        header: id.to_uppercase(),
        prompt: format!("Which {id}?"),
        options: options.iter().map(choice).collect(),
        multi_select,
        allow_custom,
        required: true,
    }
}

pub fn timeline(thread: &str, items: Vec<Item>) -> Timeline {
    Timeline { thread: ThreadId::new(thread), detail: None, items, earlier: 0 }
}

pub fn new_app(open: Option<&str>) -> App {
    App::new(open.map(String::from), BuildId("test-build".into()), SendId::new("seed"))
}

pub fn app_in_project(view: ProjectView) -> App {
    let mut app = new_app(None);
    app.update(Event::Server(ServerMsg::Welcome));
    let projects = vec![machine(view.machine.as_str(), MachineStatus::Connected, vec![view.project.clone()])];
    app.update(Event::Server(ServerMsg::Reply { id: CallId(1), result: Ok(Reply::Projects(projects)) }));
    app.update(Event::Key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter)));
    app.update(Event::Server(ServerMsg::Project(Arc::new(view))));
    assert_eq!(app.link, Link::Up);
    app
}
