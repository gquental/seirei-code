use crate::ids::{MachineId, ModelId, ProjectId, ProjectRef, ProviderInstanceId, RequestId, T3Time, ThreadId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::SystemTime};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub title: String,
    pub root: PathBuf,
    pub default_model: Option<ModelSelection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub id: MachineId,
    /// Where seirei reaches its T3, for display.
    pub address: String,
    /// T3's own name for the server, once seirei has reached it.
    pub label: Option<String>,
    pub status: MachineStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    Connected,
    /// No credential, a lapsed one, or one T3 rejected.
    NeedsLogin,
    /// T3 did not answer.
    Unreachable {
        detail: String,
    },
    /// T3 answered, but not with its projects.
    Failed {
        detail: String,
    },
    /// The same T3 as `of`, which comes first in `MachineId` order, whether or not `of` answers
    /// now. It lists no projects and takes no commands, so each project has one coordinator.
    Duplicate {
        of: MachineId,
    },
    /// Connected, but to another T3 than this address reached before. Seirei forgot the
    /// coordinators it knew on the old one.
    Replaced {
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineProjects {
    pub machine: Machine,
    pub projects: Vec<Project>,
}

/// Finds the project `name` names, written `[machine/]project` with the project by title or
/// id. A bare name must match on exactly one machine.
pub fn find_project(listing: &[MachineProjects], name: &str) -> Result<ProjectRef, String> {
    let named = |project: &Project, wanted: &str| project.title == wanted || project.id.as_str() == wanted;
    let qualified = name.split_once('/').and_then(|(machine, wanted)| {
        let listed = listing.iter().find(|listed| listed.machine.id.as_str() == machine)?;
        Some((listed, wanted))
    });
    if let Some((listed, wanted)) = qualified {
        let machine = &listed.machine;
        return match listed.projects.iter().find(|project| named(project, wanted)) {
            Some(project) => Ok(ProjectRef { machine: machine.id.clone(), project: project.id.clone() }),
            None if matches!(machine.status, MachineStatus::Connected | MachineStatus::Replaced { .. }) => {
                Err(format!("{} has no project titled {wanted:?} or with that id", machine.id))
            }
            None => Err(format!("{} lists no projects: {}", machine.id, machine.status_text())),
        };
    }
    let found: Vec<ProjectRef> = (listing.iter())
        .flat_map(|listed| listed.projects.iter().map(move |project| (&listed.machine, project)))
        .filter(|(_, project)| named(project, name))
        .map(|(machine, project)| ProjectRef { machine: machine.id.clone(), project: project.id.clone() })
        .collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("no machine has a project titled {name:?} or with that id")),
        several => {
            let names: Vec<String> = several.iter().map(|at| format!("{}/{name}", at.machine)).collect();
            Err(format!("{name:?} is on more than one machine; name one of {}", names.join(", ")))
        }
    }
}

/// Where a pairing code for another machine's T3 comes from. Seirei can run only the local `t3`.
pub const PAIRING_HINT: &str =
    "In T3 on that machine, create a pairing code (or run `t3 auth pairing create --ttl 10m` there)";

/// The command that pairs seirei with `machine`'s T3.
pub fn login_command(machine: &MachineId) -> String {
    if machine.is_local() { "seirei login".into() } else { format!("seirei login {machine}") }
}

impl Machine {
    /// The status in words, with what would fix it.
    pub fn status_text(&self) -> String {
        match &self.status {
            MachineStatus::Connected => "connected".into(),
            MachineStatus::NeedsLogin => format!("needs login: run `{}`", login_command(&self.id)),
            MachineStatus::Unreachable { detail } => format!("unreachable: {detail}"),
            MachineStatus::Failed { detail } => format!("failed: {detail}"),
            MachineStatus::Duplicate { of } => format!("the same T3 as {of}, so its projects are listed there"),
            MachineStatus::Replaced { detail } => format!("connected, but {detail}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider: ProviderInstanceId,
    pub model: ModelId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Preparing,
    Queued,
    Starting,
    Running,
    Waiting,
    Completed,
    Interrupted,
    Failed,
    Cancelled,
    RolledBack,
}

impl ThreadStatus {
    pub fn is_live(self) -> bool {
        use ThreadStatus::*;
        match self {
            Preparing | Queued | Starting | Running | Waiting => true,
            Idle | Completed | Interrupted | Failed | Cancelled | RolledBack => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    ReadOnly,
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

impl Access {
    pub const ALL: [Self; 5] =
        [Self::ReadOnly, Self::ApprovalRequired, Self::AutoAcceptEdits, Self::Auto, Self::FullAccess];

    pub fn allows(self, mode: RuntimeMode) -> bool {
        self >= Self::from(mode)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ApprovalRequired => "approval-required",
            Self::AutoAcceptEdits => "auto-accept-edits",
            Self::Auto => "auto",
            Self::FullAccess => "full-access",
        }
    }
}

impl From<RuntimeMode> for Access {
    fn from(mode: RuntimeMode) -> Self {
        match mode {
            RuntimeMode::ApprovalRequired => Self::ApprovalRequired,
            RuntimeMode::AutoAcceptEdits => Self::AutoAcceptEdits,
            RuntimeMode::Auto => Self::Auto,
            RuntimeMode::FullAccess => Self::FullAccess,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Lineage {
    Root { via_mcp: bool },
    Subagent { parent: ThreadId },
    Fork { parent: ThreadId },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub title: String,
    pub status: ThreadStatus,
    pub lineage: Lineage,
    pub runtime_mode: RuntimeMode,
    pub model: ModelSelection,
    pub item_count: u64,
    pub settled: bool,
    pub created_at: T3Time,
    pub updated_at: T3Time,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobLink {
    Subagent,
    /// A top-level `t3_thread_launch` thread. T3 records no parent for it, so seirei attributes
    /// it by the coordinator's job tag in its title.
    Launched,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub thread: ThreadSummary,
    pub link: JobLink,
    /// Every thread descended from this job through `delegate_task`, at any depth, newest first.
    pub subtasks: Vec<ThreadSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CoordinatorView {
    Absent,
    Starting,
    Ready(ThreadSummary),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectView {
    pub machine: MachineId,
    pub project: Project,
    pub coordinator: CoordinatorView,
    /// Live jobs first, then most recently updated.
    pub jobs: Vec<Job>,
    pub questions: Vec<PendingRequest>,
    pub health: Health,
    /// The grant of the credential `seirei login` saved. `None` when the daemon's token came from
    /// `$SEIREI_T3_TOKEN` or the config file: its grant is unknown, so T3 alone decides.
    pub access: Option<Access>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub last_sync: Option<SystemTime>,
    pub problem: Option<Problem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Problem {
    Unreachable { detail: String },
    Unauthorized,
    Failed { detail: String },
    Unparsed { count: usize, sample: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingRequest {
    pub id: RequestId,
    pub thread: ThreadId,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub prompt: String,
    pub options: Vec<Choice>,
    pub multi_select: bool,
    pub allow_custom: bool,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    pub description: String,
    /// What T3 expects back when this choice is picked; the label when absent.
    pub value: Option<String>,
}

/// Answers keyed by `Question::id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Answers(pub BTreeMap<String, Answer>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Answer {
    /// Indexes into `Question::options`.
    Picked(Vec<usize>),
    Custom(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_allows_the_modes_at_or_below_it() {
        use RuntimeMode::*;
        let allowed = |access: Access| -> Vec<RuntimeMode> {
            [ApprovalRequired, AutoAcceptEdits, Auto, FullAccess].into_iter().filter(|m| access.allows(*m)).collect()
        };
        assert_eq!(allowed(Access::ReadOnly), []);
        assert_eq!(allowed(Access::ApprovalRequired), [ApprovalRequired]);
        assert_eq!(allowed(Access::Auto), [ApprovalRequired, AutoAcceptEdits, Auto]);
        assert_eq!(allowed(Access::FullAccess), [ApprovalRequired, AutoAcceptEdits, Auto, FullAccess]);
    }

    fn listed(machine: &str, status: MachineStatus, titles: &[&str]) -> MachineProjects {
        let id = MachineId::parse(machine).expect("a machine name");
        let projects = (titles.iter())
            .map(|title| Project {
                id: ProjectId::new(format!("{machine}-{title}")),
                title: title.to_string(),
                root: PathBuf::from(format!("/src/{title}")),
                default_model: None,
            })
            .collect();
        MachineProjects { machine: Machine { id, address: String::new(), label: None, status }, projects }
    }

    #[test]
    fn a_project_is_found_by_title_or_id_and_qualified_when_ambiguous() {
        let listing = [
            listed("local", MachineStatus::Connected, &["seirei-code", "notes"]),
            listed("macbook", MachineStatus::Connected, &["seirei-code", "photos"]),
            listed("studio", MachineStatus::NeedsLogin, &[]),
        ];
        let at = |machine: &str, project: &str| ProjectRef {
            machine: MachineId::parse(machine).unwrap(),
            project: ProjectId::new(project),
        };
        assert_eq!(find_project(&listing, "photos"), Ok(at("macbook", "macbook-photos")));
        assert_eq!(find_project(&listing, "local-notes"), Ok(at("local", "local-notes")), "by id");
        assert_eq!(find_project(&listing, "macbook/seirei-code"), Ok(at("macbook", "macbook-seirei-code")));
        assert_eq!(find_project(&listing, "local/local-notes"), Ok(at("local", "local-notes")));

        let ambiguous = find_project(&listing, "seirei-code").expect_err("on two machines");
        assert!(ambiguous.contains("local/seirei-code, macbook/seirei-code"), "{ambiguous}");
        let missing = find_project(&listing, "macbook/notes").expect_err("notes is local");
        assert_eq!(missing, "macbook has no project titled \"notes\" or with that id");
        let locked = find_project(&listing, "studio/anything").expect_err("studio lists nothing");
        assert!(locked.contains("needs login"), "{locked}");
        assert!(find_project(&listing, "nowhere").is_err());
        assert!(find_project(&listing, "elsewhere/notes").is_err(), "an unknown machine qualifies nothing");
    }

    #[test]
    fn an_access_name_is_its_serde_name() {
        for access in Access::ALL {
            assert_eq!(serde_json::to_value(access).expect("serializes"), access.name());
        }
    }
}
