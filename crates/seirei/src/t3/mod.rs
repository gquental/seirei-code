#[cfg(test)]
pub mod fake;
pub mod login;
mod mcp;
mod wire;

pub use mcp::{McpT3, origin};

use crate::config::{Endpoint, Token};
use login::{Credentials, LoginError, PairingCode};
use seirei_proto::{
    ids::{ItemId, ProjectId, RequestId, SendId, ThreadId},
    model::{Access, Answers, ModelSelection, PendingRequest, Project, ThreadSummary},
    timeline::{Item, ThreadDetail},
};
use std::{collections::BTreeMap, future::Future};

pub trait T3Api: Send + Sync + 'static {
    /// Reaches the T3 at `endpoint`. Nothing goes out before the first call.
    fn connect(endpoint: Endpoint, token: Option<Token>) -> Self
    where
        Self: Sized;

    fn access(&self) -> Option<Access>;

    /// Pairs with this T3 through its pairing-code OAuth flow. The credential comes back unused.
    fn pair(&self, access: Access, code: PairingCode) -> impl Future<Output = Result<Credentials, LoginError>> + Send;

    /// Every later call carries `token`. A call already in flight finishes with the old one.
    fn use_token(&self, token: Token);

    /// Where this T3 is reached, for display.
    fn address(&self) -> String;

    /// T3's own identity. Two addresses can reach one T3, and one host can run two T3s.
    fn environment(&self) -> impl Future<Output = Result<Environment, T3Error>> + Send;

    fn projects(&self) -> impl Future<Output = Result<Vec<Project>, T3Error>> + Send;

    fn models(&self) -> impl Future<Output = Result<Vec<ModelSelection>, T3Error>> + Send;

    /// One page of `t3_thread_list`, newest `updatedAt` first (ties by id, descending). `cursor`
    /// is an offset into that order, so rows shift between pages when threads are bumped.
    fn threads(
        &self,
        project: &ProjectId,
        cursor: Option<u64>,
    ) -> impl Future<Output = Result<ThreadPage, T3Error>> + Send;

    /// Every thread of the project whose title contains `needle`. T3 never regenerates a title
    /// set at launch, so a tag in the title finds its thread in one call.
    fn threads_titled(
        &self,
        project: &ProjectId,
        needle: &str,
    ) -> impl Future<Output = Result<Vec<ThreadSummary>, T3Error>> + Send;

    fn read(&self, thread: &ThreadId, query: ReadQuery) -> impl Future<Output = Result<TimelinePage, T3Error>> + Send;

    /// Creates a top-level thread at the caller's runtime-mode ceiling. T3 offers no retry key,
    /// so the caller makes the surrounding operation idempotent (see `coordinator::ensure_step`).
    fn launch(&self, launch: Launch) -> impl Future<Output = Result<ThreadId, T3Error>> + Send;

    /// Idempotent per `id` (T3's `clientRequestId`) for as long as the credential lives.
    fn send(&self, thread: &ThreadId, text: &str, id: &SendId) -> impl Future<Output = Result<(), T3Error>> + Send;

    /// Open user-input questions only. Tool approvals never appear here.
    fn pending(&self, thread: &ThreadId) -> impl Future<Output = Result<Vec<RequestId>, T3Error>> + Send;

    fn question(
        &self,
        thread: &ThreadId,
        request: &RequestId,
    ) -> impl Future<Output = Result<PendingRequest, T3Error>> + Send;

    fn answer(&self, question: &PendingRequest, answers: &Answers) -> impl Future<Output = Result<(), T3Error>> + Send;
}

/// T3's `limit`. Above 100 T3 rejects the whole call, so an out-of-range size cannot be built.
#[derive(Clone, Copy, Debug)]
pub struct PageSize(u8);

impl PageSize {
    pub const MAX: Self = Self(100);

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// An item's index in T3's current ordering of a thread. Only meaningful within one read:
/// T3 recomputes positions every time, so nothing outside this boundary keeps them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Position(pub u64);

pub struct ReadQuery {
    /// T3's `afterPosition`, exclusive. `None` reads from the first item.
    pub after: Option<Position>,
    pub limit: PageSize,
    pub max_chars: u32,
}

pub struct ThreadPage {
    pub threads: Vec<ThreadSummary>,
    pub next: Option<u64>,
    pub unparsed: BTreeMap<ThreadId, String>,
}

pub struct TimelinePage {
    pub detail: ThreadDetail,
    pub items: Vec<Item>,
    /// How many items precede the page in the thread.
    pub earlier: u64,
    pub has_more: bool,
    pub unparsed: BTreeMap<ItemId, String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub environment_id: String,
    pub label: String,
}

pub struct Launch {
    pub project: ProjectId,
    pub title: String,
    pub message: String,
    /// `None` runs the thread on the project's default model. T3 rejects that when the project
    /// has no default.
    pub model: Option<ModelSelection>,
}

#[derive(Debug, thiserror::Error)]
pub enum T3Error {
    #[error("T3 is unreachable: {0}")]
    Unreachable(String),
    #[error("lost contact with T3 mid-call: {0}")]
    Lost(String),
    #[error("T3 rejected seirei's credential; run `seirei login`")]
    Unauthorized,
    #[error("T3 rejected the call: {0}")]
    Rejected(String),
    #[error("T3 sent something this build cannot parse: {0}")]
    Malformed(String),
    /// Opening the MCP session failed this way, so the tool call never left.
    #[error("could not open an MCP session with T3: {0}")]
    NoSession(Box<T3Error>),
}

impl T3Error {
    pub fn may_have_applied(&self) -> bool {
        match self {
            Self::Lost(_) | Self::Malformed(_) => true,
            Self::Unreachable(_) | Self::Unauthorized | Self::Rejected(_) | Self::NoSession(_) => false,
        }
    }
}
