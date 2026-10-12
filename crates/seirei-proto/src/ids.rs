use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, fmt, sync::Arc};

macro_rules! opaque_id {
    ($($(#[$meta:meta])* $name:ident;)*) => {$(
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Debug)]
        #[serde(transparent)]
        pub struct $name(Arc<str>);

        impl $name {
            pub fn new(raw: impl Into<Arc<str>>) -> Self {
                Self(raw.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    )*};
}

opaque_id! {
    ProjectId;
    /// A T3 thread. Includes UUIDs, `mcp:<uuid>`, and percent-encoded provider paths: echo it
    /// verbatim, never decode or split it.
    ThreadId;
    ItemId;
    RunId;
    RequestId;
    ProviderInstanceId;
    ModelId;
}

/// The name seirei uses for one T3 server. `local` is the T3 on this machine; every other name
/// is the user's. Ordered `local` first, then by name: the order seirei lists machines in, and
/// the order that decides which of two names for one T3 is the duplicate.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
#[serde(try_from = "String", into = "String")]
pub struct MachineId(Arc<str>);

impl MachineId {
    pub const LOCAL: &str = "local";

    pub fn local() -> Self {
        Self(Self::LOCAL.into())
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut chars = raw.chars();
        let lead = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if lead && rest && raw.len() <= 32 {
            Ok(Self(raw.into()))
        } else {
            Err(format!(
                "{raw:?} is not a machine name: use 1 to 32 lowercase letters, digits, and dashes, not starting with a dash"
            ))
        }
    }

    pub fn is_local(&self) -> bool {
        &*self.0 == Self::LOCAL
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Ord for MachineId {
    fn cmp(&self, other: &Self) -> Ordering {
        (!self.is_local(), &self.0).cmp(&(!other.is_local(), &other.0))
    }
}

impl PartialOrd for MachineId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for MachineId {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, String> {
        Self::parse(&raw)
    }
}

impl From<MachineId> for String {
    fn from(id: MachineId) -> Self {
        id.0.as_ref().into()
    }
}

/// A project on one machine. T3 ids are unique only within one T3, so every project the daemon
/// syncs is keyed by both. Threads stay plain ids: they only travel inside a project's scope.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Debug)]
pub struct ProjectRef {
    pub machine: MachineId,
    pub project: ProjectId,
}

impl fmt::Display for ProjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.machine, self.project.as_str())
    }
}

/// Client-minted idempotency key for one submitted draft, passed to `t3_thread_send` as
/// `clientRequestId`. The TUI keeps the key until the daemon confirms the send, so resending
/// after a lost reply or a daemon crash never duplicates the message.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct SendId(Arc<str>);

impl SendId {
    /// `raw` must be unique per message: T3 drops a second send under the same key as a duplicate.
    pub fn new(raw: impl Into<Arc<str>>) -> Self {
        Self(raw.into())
    }

    pub fn fresh() -> Self {
        use std::hash::{BuildHasher, RandomState};
        let a = RandomState::new().hash_one(std::time::SystemTime::now());
        let b = RandomState::new().hash_one(std::process::id());
        Self(format!("seirei-{a:016x}{b:016x}").into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A T3 server timestamp as Unix milliseconds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct T3Time(pub i64);

/// Offset into an item's text in UTF-16 code units, as T3's `textOffset` counts them.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct Utf16Offset(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_name_is_short_lowercase_and_dashed() {
        for valid in ["local", "macbook", "mac-studio", "box2", "9", &"a".repeat(32)] {
            assert_eq!(MachineId::parse(valid).map(|id| id.to_string()), Ok(valid.to_owned()));
        }
        for invalid in ["", "-macbook", "MacBook", "mac_studio", "mac studio", "studio/x", "é", &"a".repeat(33)] {
            assert!(MachineId::parse(invalid).is_err(), "{invalid:?} is not a machine name");
        }
        let json = serde_json::to_string(&MachineId::local()).expect("serializes");
        assert_eq!(json, "\"local\"");
        assert!(serde_json::from_str::<MachineId>("\"Not A Name\"").is_err(), "parsed on the way in too");
    }

    #[test]
    fn machines_sort_local_first_then_by_name() {
        let mut ids: Vec<MachineId> =
            ["studio", "local", "alpha", "macbook"].into_iter().map(|raw| MachineId::parse(raw).unwrap()).collect();
        ids.sort();
        let names: Vec<&str> = ids.iter().map(MachineId::as_str).collect();
        assert_eq!(names, ["local", "alpha", "macbook", "studio"]);
    }
}
