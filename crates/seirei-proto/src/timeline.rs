use crate::{
    ids::{ItemId, RunId, T3Time, ThreadId, Utf16Offset},
    model::ThreadSummary,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub kind: ItemKind,
    pub status: ItemStatus,
    pub run: Option<RunId>,
    pub title: Option<String>,
    pub text: ItemText,
    pub updated_at: T3Time,
}

macro_rules! open_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $raw:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(from = "String", into = "String")]
        pub enum $name { $($(#[$vmeta])* $variant,)* Other(Box<str>) }

        impl From<String> for $name {
            fn from(raw: String) -> Self {
                match raw.as_str() { $($raw => Self::$variant,)* _ => Self::Other(raw.into()) }
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                match value { $($name::$variant => $raw.into(),)* $name::Other(raw) => raw.into() }
            }
        }
    };
}

open_enum! {
    ItemKind {
        UserMessage = "user_message",
        AssistantMessage = "assistant_message",
        Reasoning = "reasoning",
        CommandExecution = "command_execution",
        DynamicTool = "dynamic_tool",
        FileChange = "file_change",
        UserInputRequest = "user_input_request",
        /// A tool permission the user must grant in T3's own UI: no MCP tool can approve it.
        ApprovalRequest = "approval_request",
        Subagent = "subagent",
        Checkpoint = "checkpoint",
        Notification = "notification",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Idle,
    Pending,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemText {
    pub shown: String,
    pub more_at: Option<Utf16Offset>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadDetail {
    pub summary: ThreadSummary,
    pub branch: Option<String>,
    pub worktree: Option<PathBuf>,
    pub active_run: Option<RunId>,
    pub pending_requests: u32,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    pub thread: ThreadId,
    pub detail: Option<ThreadDetail>,
    /// The thread's newest items, oldest first.
    pub items: Vec<Item>,
    /// How many items precede `items` in the thread.
    pub earlier: u64,
}

impl Timeline {
    pub fn empty(thread: ThreadId) -> Self {
        Self { thread, detail: None, items: Vec::new(), earlier: 0 }
    }
}
