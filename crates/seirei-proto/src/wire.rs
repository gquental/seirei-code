//! The daemon socket protocol: newline-delimited JSON over a Unix socket.
//!
//! Convergence rule: every `Project` and `Timeline` frame carries the complete current value of
//! that topic, and `Subscribe` is declarative. A client that reconnects sends `Hello` and its
//! current `Subscribe`, and the first frames it gets back are the full state. Nothing is
//! replayed, sequenced, or diffed.

use crate::{
    ids::{MachineId, ProjectRef, RequestId, SendId, ThreadId},
    model::{Access, Answers, MachineProjects, ProjectView},
    timeline::Timeline,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

pub const MAX_LINE: usize = 16 << 20;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct BuildId(pub String);

impl BuildId {
    /// The executable's identity when this process first asked, so a rebuild on disk never
    /// changes what a running TUI or daemon claims to be.
    pub fn current() -> Self {
        static CURRENT: OnceLock<BuildId> = OnceLock::new();
        let current = CURRENT.get_or_init(|| {
            let stamp = std::env::current_exe()
                .and_then(std::fs::metadata)
                .map(|m| format!("{}:{:?}", m.len(), m.modified().ok()))
                .unwrap_or_default();
            Self(format!("{}+{stamp}", env!("CARGO_PKG_VERSION")))
        });
        current.clone()
    }
}

/// Correlates a `Call` with its `Reply`. Allocated by the client, unique per connection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct CallId(pub u64);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientMsg {
    /// First line on every connection. Its shape is frozen across builds. `take_over` asks a
    /// daemon of a different build to exit so this client can start its own.
    Hello {
        build: BuildId,
        take_over: bool,
    },
    Subscribe(Subscription),
    Call {
        id: CallId,
        command: Command,
    },
}

/// What the client is looking at. The coordinator's timeline streams with the project; `job`
/// adds one job timeline.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub project: Option<ProjectRef>,
    pub job: Option<ThreadId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    /// Every machine with its status, and the projects of those seirei reached.
    ListProjects,
    /// Idempotent: returns the existing coordinator, adopts one whose launch was never recorded,
    /// or launches one.
    EnsureCoordinator {
        project: ProjectRef,
    },
    /// Idempotent per `send_id`.
    Send {
        project: ProjectRef,
        thread: ThreadId,
        text: String,
        send_id: SendId,
    },
    Answer {
        project: ProjectRef,
        request: RequestId,
        answers: Answers,
    },
    /// Reach the T3 at `address` under `name`. The daemon parses the address. Adding a name at
    /// the address it already has changes nothing.
    AddMachine {
        name: MachineId,
        address: String,
    },
    /// Stop reaching `machine`. Its credential and records stay on disk.
    RemoveMachine {
        machine: MachineId,
    },
    /// Pair with `machine`'s T3 and use the credential at once. Without a code the daemon makes
    /// one with the local `t3`, so only `local` may leave it out.
    Login {
        machine: MachineId,
        access: Access,
        code: Option<String>,
    },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMsg {
    /// Frozen across builds, like `Hello`.
    Welcome,
    /// Frozen. The daemon keeps running; the client decides whether to retry with `take_over`.
    WrongBuild {
        daemon: BuildId,
    },
    /// Frozen. The daemon is exiting because a client of another build took over.
    Yielding,
    Project(Arc<ProjectView>),
    Timeline(Arc<Timeline>),
    Reply {
        id: CallId,
        result: Result<Reply, CallError>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Done,
    Projects(Vec<MachineProjects>),
}

/// Why a call failed, phrased for the user. `retry_safe` is true whenever repeating the exact
/// same call cannot double-apply it. A `Send` always is, because T3 drops a repeat of its
/// `SendId`. A failed `Answer` or `EnsureCoordinator` is only when T3 cannot have applied it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallError {
    pub message: String,
    pub retry_safe: bool,
}

pub fn encode(msg: &impl Serialize) -> Vec<u8> {
    let mut line = serde_json::to_vec(msg).expect("protocol types serialize infallibly");
    line.push(b'\n');
    line
}

pub fn decode<T: DeserializeOwned>(line: &[u8]) -> serde_json::Result<T> {
    serde_json::from_slice(line)
}

pub fn socket_path() -> PathBuf {
    let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(path) = var("SEIREI_SOCKET") {
        return path.into();
    }
    if let Some(dir) = var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("seirei/daemon.sock");
    }
    std::env::temp_dir().join(format!("seirei-{}", user_id())).join("daemon.sock")
}

fn user_id() -> String {
    use std::os::unix::fs::MetadataExt;
    (std::env::var("UID").ok().filter(|uid| !uid.is_empty()))
        .or_else(|| std::env::home_dir().and_then(|home| home.metadata().ok()).map(|m| m.uid().to_string()))
        .unwrap_or_else(|| "user".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, time::Duration};

    #[test]
    fn the_build_id_survives_a_rebuild_on_disk() {
        let startup = BuildId::current();
        let exe = File::open(std::env::current_exe().unwrap()).unwrap();
        let built = exe.metadata().unwrap().modified().unwrap();
        exe.set_modified(built + Duration::from_secs(1)).unwrap();
        let after = BuildId::current();
        exe.set_modified(built).unwrap();
        assert_eq!(after, startup, "a running process keeps the identity it started with");
    }
}
