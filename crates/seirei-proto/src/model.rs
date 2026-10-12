use crate::ids::{ModelId, ProjectId, ProviderInstanceId, RequestId, T3Time, ThreadId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::SystemTime};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub title: String,
    pub root: PathBuf,
    pub default_model: Option<ModelSelection>,
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

    #[test]
    fn an_access_name_is_its_serde_name() {
        for access in Access::ALL {
            assert_eq!(serde_json::to_value(access).expect("serializes"), access.name());
        }
    }
}
