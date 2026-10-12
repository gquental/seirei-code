use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
