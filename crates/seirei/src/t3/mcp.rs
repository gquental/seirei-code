use super::{Launch, PageSize, ReadQuery, T3Api, T3Error, ThreadPage, TimelinePage, wire};
use crate::config::Token;
use seirei_proto::{
    ids::{ProjectId, RequestId, SendId, ThreadId},
    model::{Access, Answers, ModelSelection, PendingRequest, Project, ThreadSummary},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const PROTOCOL: &str = "2025-06-18";
const SESSION_HEADER: &str = "mcp-session-id";

pub struct McpT3 {
    http: reqwest::Client,
    t3_home: PathBuf,
    /// A 401 is reported as `Unauthorized`, never retried: only `seirei login` mints a token,
    /// because a CLI-issued session is capped at `approval-required`. Without a token, calls still
    /// go out, so a T3 that is down reads as unreachable rather than as a missing login.
    token: Option<Token>,
    session: tokio::sync::Mutex<Option<Session>>,
    next_id: AtomicU64,
}

#[derive(Clone, PartialEq)]
struct Session {
    endpoint: String,
    id: String,
}

enum Posted {
    Answer { session: Option<String>, body: Value },
    Accepted,
    SessionGone,
}

#[derive(Deserialize)]
struct RuntimeFile {
    origin: String,
}

pub fn origin(t3_home: &Path) -> Result<String, T3Error> {
    let path = t3_home.join("userdata/server-runtime.json");
    let text = std::fs::read_to_string(&path).map_err(|e| T3Error::Unreachable(format!("{}: {e}", path.display())))?;
    let runtime: RuntimeFile =
        serde_json::from_str(&text).map_err(|e| T3Error::Unreachable(format!("{}: {e}", path.display())))?;
    Ok(runtime.origin.trim_end_matches('/').to_owned())
}

impl McpT3 {
    pub fn new(t3_home: PathBuf, token: Option<Token>) -> Self {
        let http = (reqwest::Client::builder())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("a client without TLS or proxies always builds");
        Self { http, t3_home, token, session: Default::default(), next_id: AtomicU64::new(1) }
    }

    /// Null arguments are dropped: T3's validators reject an explicit `null` for an optional
    /// parameter even where the published schema allows it.
    async fn call(&self, tool: &'static str, mut args: Value) -> Result<Value, T3Error> {
        if let Value::Object(fields) = &mut args {
            fields.retain(|_, value| !value.is_null());
        }
        let request = self.rpc("tools/call", json!({ "name": tool, "arguments": args }));
        let (session, reused) = self.session(None).await?;
        let posted = match self.post(&session.endpoint, Some(&session.id), &request).await {
            Ok(Posted::SessionGone) | Err(T3Error::Unreachable(_)) if reused => {
                let (session, _) = self.session(Some(&session)).await?;
                self.post(&session.endpoint, Some(&session.id), &request).await?
            }
            posted => posted?,
        };
        match posted {
            Posted::Answer { body, .. } => tool_result(body),
            Posted::Accepted => Err(T3Error::Malformed(format!("{tool}: 202 without a result"))),
            Posted::SessionGone => Err(T3Error::Rejected("T3 dropped a session it had just created".into())),
        }
    }

    /// The cached session unless it is `stale`, else a new one, and whether it was cached. The
    /// lock is held across the connect, so concurrent callers share one rebuild.
    async fn session(&self, stale: Option<&Session>) -> Result<(Session, bool), T3Error> {
        let mut cached = self.session.lock().await;
        if let Some(session) = cached.as_ref().filter(|&session| Some(session) != stale) {
            return Ok((session.clone(), true));
        }
        let session = self.connect().await.map_err(|cause| T3Error::NoSession(Box::new(cause)))?;
        *cached = Some(session.clone());
        Ok((session, false))
    }

    async fn connect(&self) -> Result<Session, T3Error> {
        let endpoint = format!("{}/mcp", origin(&self.t3_home)?);
        let hello = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": { "name": "seirei", "version": env!("CARGO_PKG_VERSION") },
        });
        let id = match self.post(&endpoint, None, &self.rpc("initialize", hello)).await? {
            Posted::Answer { session: Some(id), body } if body.get("result").is_some() => id,
            Posted::Answer { body, .. } => {
                return Err(T3Error::Malformed(format!("initialize opened no session: {body}")));
            }
            Posted::Accepted | Posted::SessionGone => {
                return Err(T3Error::Malformed("initialize opened no session".into()));
            }
        };
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.post(&endpoint, Some(&id), &initialized).await?;
        Ok(Session { endpoint, id })
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    async fn post(&self, endpoint: &str, session: Option<&str>, body: &Value) -> Result<Posted, T3Error> {
        let mut request = (self.http.post(endpoint))
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .json(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(&token.secret.0);
        }
        if let Some(id) = session {
            request = request.header(SESSION_HEADER, id).header("mcp-protocol-version", PROTOCOL);
        }
        let response = request.send().await.map_err(|e| {
            let detail = format!("{endpoint}: {e}");
            if e.is_connect() { T3Error::Unreachable(detail) } else { T3Error::Lost(detail) }
        })?;
        let status = response.status();
        let header = response.headers().get(SESSION_HEADER).and_then(|v| v.to_str().ok()).map(String::from);
        let bytes = response.bytes().await.map_err(|e| T3Error::Lost(format!("{endpoint}: {e}")))?;
        let text = || String::from_utf8_lossy(&bytes).into_owned();
        match status.as_u16() {
            401 => Err(T3Error::Unauthorized),
            404 if session.is_some() => Ok(Posted::SessionGone),
            202 => Ok(Posted::Accepted),
            200..=299 => serde_json::from_slice(&bytes)
                .map(|body| Posted::Answer { session: header, body })
                .map_err(|e| T3Error::Malformed(format!("{e}: {}", text()))),
            400..=499 => Err(T3Error::Rejected(format!("HTTP {status}: {}", text()))),
            _ => Err(T3Error::Lost(format!("HTTP {status}: {}", text()))),
        }
    }
}

/// T3 reports a tool failure as `isError: true` with a text block holding `{"_tag","code","message"}`;
/// that becomes `Rejected("<code>: <message>")`.
fn tool_result(mut body: Value) -> Result<Value, T3Error> {
    if let Some(error) = body.get("error") {
        return Err(T3Error::Rejected(error.get("message").and_then(Value::as_str).unwrap_or("JSON-RPC error").into()));
    }
    let mut result = body.get_mut("result").map(Value::take).ok_or_else(|| T3Error::Malformed(body.to_string()))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = result.pointer("/content/0/text").and_then(Value::as_str).unwrap_or_default();
        let failure: Value = serde_json::from_str(text).unwrap_or_default();
        return Err(T3Error::Rejected(match (failure.get("code"), failure.get("message")) {
            (Some(Value::String(code)), Some(Value::String(message))) => format!("{code}: {message}"),
            _ => text.to_owned(),
        }));
    }
    match result.get_mut("structuredContent").map(Value::take) {
        Some(content) if !content.is_null() => Ok(content),
        _ => Err(T3Error::Malformed(format!("no structuredContent in {result}"))),
    }
}

impl T3Api for McpT3 {
    fn access(&self) -> Option<Access> {
        self.token.as_ref().and_then(|token| token.access)
    }

    fn address(&self) -> String {
        origin(&self.t3_home).unwrap_or_else(|_| self.t3_home.display().to_string())
    }

    async fn projects(&self) -> Result<Vec<Project>, T3Error> {
        let mut projects = Vec::new();
        let mut cursor = None;
        loop {
            let args = json!({ "cursor": cursor, "limit": PageSize::MAX.get() });
            let page: wire::ProjectList = wire::parse(self.call("t3_project_list", args).await?)?;
            cursor = page.next_cursor;
            projects.extend(page.live());
            if cursor.is_none() {
                return Ok(projects);
            }
        }
    }

    async fn models(&self) -> Result<Vec<ModelSelection>, T3Error> {
        let capabilities: wire::Capabilities = wire::parse(self.call("orchestrator_capabilities", json!({})).await?)?;
        Ok(capabilities.runnable().collect())
    }

    async fn threads(&self, project: &ProjectId, cursor: Option<u64>) -> Result<ThreadPage, T3Error> {
        let args = json!({
            "projectId": project.as_str(),
            "cursor": cursor,
            "limit": PageSize::MAX.get(),
            "includeSubagents": true,
        });
        wire::thread_page(self.call("t3_thread_list", args).await?)
    }

    async fn threads_titled(&self, project: &ProjectId, needle: &str) -> Result<Vec<ThreadSummary>, T3Error> {
        let mut threads = Vec::new();
        let mut cursor = None;
        loop {
            let args = json!({
                "projectId": project.as_str(),
                "titleContains": needle,
                "cursor": cursor,
                "limit": PageSize::MAX.get(),
            });
            let page = wire::thread_page(self.call("t3_thread_list", args).await?)?;
            cursor = page.next;
            threads.extend(page.threads);
            if cursor.is_none() {
                return Ok(threads);
            }
        }
    }

    async fn read(&self, thread: &ThreadId, query: ReadQuery) -> Result<TimelinePage, T3Error> {
        let args = json!({
            "threadId": thread.as_str(),
            "view": "activity",
            "afterPosition": query.after.map(|p| p.0),
            "limit": query.limit.get(),
            "maxCharsPerItem": query.max_chars,
        });
        wire::timeline_page(self.call("t3_thread_read", args).await?, query.after)
    }

    async fn launch(&self, launch: Launch) -> Result<ThreadId, T3Error> {
        let model = (launch.model.as_ref())
            .map(|selection| json!({ "instanceId": selection.provider.as_str(), "model": selection.model.as_str() }));
        let args = json!({
            "projectId": launch.project.as_str(),
            "title": launch.title,
            "message": launch.message,
            "modelSelection": model,
        });
        let launched: wire::Launched = wire::parse(self.call("t3_thread_launch", args).await?)?;
        Ok(ThreadId::new(launched.thread_id))
    }

    async fn send(&self, thread: &ThreadId, text: &str, id: &SendId) -> Result<(), T3Error> {
        let args =
            json!({ "threadId": thread.as_str(), "message": text, "mode": "auto", "clientRequestId": id.as_str() });
        self.call("t3_thread_send", args).await.map(drop)
    }

    async fn pending(&self, thread: &ThreadId) -> Result<Vec<RequestId>, T3Error> {
        let list: wire::PendingList =
            wire::parse(self.call("t3_pending_request_list", json!({ "threadId": thread.as_str() })).await?)?;
        Ok(list.request_ids.into_iter().map(RequestId::new).collect())
    }

    async fn question(&self, thread: &ThreadId, request: &RequestId) -> Result<PendingRequest, T3Error> {
        let args = json!({ "threadId": thread.as_str(), "requestId": request.as_str() });
        wire::question(thread, self.call("t3_pending_request_read", args).await?)
    }

    async fn answer(&self, question: &PendingRequest, answers: &Answers) -> Result<(), T3Error> {
        let args = json!({
            "threadId": question.thread.as_str(),
            "requestId": question.id.as_str(),
            "answers": wire::answers(question, answers),
        });
        self.call("t3_pending_request_respond", args).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Secret, t3::Position};
    use std::sync::{Arc, Mutex};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    type Respond = fn(&str) -> (u16, Option<&'static str>, String);

    struct Stub {
        t3: McpT3,
        methods: Arc<Mutex<Vec<String>>>,
        t3_home: tempfile::TempDir,
    }

    async fn stub(respond: Respond) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let t3_home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t3_home.path().join("userdata")).unwrap();
        let runtime = json!({ "origin": format!("http://{}", listener.local_addr().unwrap()) });
        std::fs::write(t3_home.path().join("userdata/server-runtime.json"), runtime.to_string()).unwrap();
        let methods = Arc::new(Mutex::new(Vec::new()));
        let seen = methods.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(exchange(stream, respond, seen.clone()));
            }
        });
        let token = Token { secret: Secret("stub".into()), access: None };
        Stub { t3: McpT3::new(t3_home.path().to_path_buf(), Some(token)), methods, t3_home }
    }

    async fn exchange(mut stream: TcpStream, respond: Respond, seen: Arc<Mutex<Vec<String>>>) {
        let mut buf = Vec::new();
        let mut chunk = [0; 4096];
        let (head_len, body_len) = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let length = head.lines().find_map(|line| line.strip_prefix("content-length:"));
                break (end + 4, length.map_or(0, |n| n.trim().parse().unwrap()));
            }
        };
        while buf.len() < head_len + body_len {
            let n = stream.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
        }
        let request: Value = serde_json::from_slice(&buf[head_len..head_len + body_len]).unwrap();
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        seen.lock().unwrap().push(method.clone());
        let (status, session, body) = respond(&method);
        let session = session.map_or_else(String::new, |id| format!("{SESSION_HEADER}: {id}\r\n"));
        let response = format!(
            "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\n{session}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn a_malformed_initialize_answer_is_not_unreachable_and_applies_nothing() {
        let stub = stub(|_| (200, Some("session-1"), "<html>this is not T3</html>".into())).await;
        let question =
            PendingRequest { id: RequestId::new("request:1"), thread: ThreadId::new("t"), questions: vec![] };
        let failed = stub.t3.answer(&question, &Answers(Default::default())).await.expect_err("no session opened");
        assert!(!matches!(failed, T3Error::Unreachable(_)), "T3 answered, so it is reachable: {failed:?}");
        assert!(failed.to_string().contains("cannot parse"), "the error says what happened: {failed}");
        assert!(!failed.may_have_applied(), "the answer never left, so a retry is safe: {failed}");
        assert_eq!(*stub.methods.lock().unwrap(), ["initialize"]);
    }

    #[tokio::test]
    async fn without_a_token_a_running_t3_needs_a_login_and_a_stopped_one_is_unreachable() {
        let stub = stub(|_| (401, None, r#"{"error":"invalid_mcp_credential"}"#.into())).await;
        let anonymous = McpT3::new(stub.t3_home.path().to_path_buf(), None);
        let refused = anonymous.projects().await.expect_err("T3 wants a credential");
        assert!(
            matches!(refused, T3Error::NoSession(ref cause) if matches!(**cause, T3Error::Unauthorized)),
            "{refused:?}"
        );
        assert_eq!(*stub.methods.lock().unwrap(), ["initialize"], "the call went out without a token");

        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let runtime = json!({ "origin": format!("http://{closed}") });
        std::fs::write(stub.t3_home.path().join("userdata/server-runtime.json"), runtime.to_string()).unwrap();
        let stopped = anonymous.projects().await.expect_err("nothing listens");
        assert!(
            matches!(stopped, T3Error::NoSession(ref cause) if matches!(**cause, T3Error::Unreachable(_))),
            "{stopped:?}"
        );
    }

    #[test]
    fn tool_failures_become_rejections_with_t3s_code() {
        let denied = json!({ "jsonrpc": "2.0", "id": 3, "result": {
            "content": [{ "type": "text", "text": "{\"_tag\":\"OrchestratorMcpFailure\",\"code\":\"capability_denied\",\"message\":\"orchestration:operate is required.\"}" }],
            "isError": true,
        }});
        match tool_result(denied) {
            Err(T3Error::Rejected(why)) => assert_eq!(why, "capability_denied: orchestration:operate is required."),
            other => panic!("a capability denial is a rejection, got {other:?}"),
        }
        let invalid = json!({ "jsonrpc": "2.0", "id": 4, "error": { "code": -32602, "message": "limit above 100" } });
        assert!(matches!(tool_result(invalid), Err(T3Error::Rejected(why)) if why == "limit above 100"));
    }

    #[test]
    fn a_successful_call_yields_its_structured_content() {
        let ok = json!({ "jsonrpc": "2.0", "id": 5, "result": {
            "content": [{ "type": "text", "text": "{\"requestIds\":[]}" }],
            "structuredContent": { "requestIds": [] },
            "isError": false,
        }});
        assert_eq!(tool_result(ok).expect("structured content"), json!({ "requestIds": [] }));
    }

    #[tokio::test]
    async fn concurrent_calls_share_one_session() {
        let stub = stub(|method| match method {
            "initialize" => (200, Some("session-1"), json!({ "jsonrpc": "2.0", "id": 1, "result": {} }).to_string()),
            "notifications/initialized" => (202, None, String::new()),
            _ => {
                let result = json!({ "structuredContent": { "requestIds": [] }, "isError": false });
                (200, None, json!({ "jsonrpc": "2.0", "id": 2, "result": result }).to_string())
            }
        })
        .await;
        let thread = ThreadId::new("t");
        let pending = || stub.t3.pending(&thread);
        let answers = tokio::join!(pending(), pending(), pending(), pending());
        for answer in [answers.0, answers.1, answers.2, answers.3] {
            assert!(answer.expect("the call succeeds").is_empty());
        }
        let methods = stub.methods.lock().unwrap().clone();
        assert_eq!(methods.iter().filter(|m| *m == "initialize").count(), 1, "one caller opened it: {methods:?}");
        assert_eq!(methods.iter().filter(|m| *m == "tools/call").count(), 4);
    }

    #[tokio::test]
    #[ignore = "needs a running T3 and a token in SEIREI_T3_TOKEN"]
    async fn live_scan_and_tail_read_parse() {
        let config = crate::config::Config::load().expect("the config loads");
        let local = config.machine(&seirei_proto::ids::MachineId::local()).expect("local is always configured");
        let t3 = McpT3::new(local.t3_home.clone(), local.token.clone());
        let mut busiest: Option<ThreadSummary> = None;
        for project in t3.projects().await.expect("projects list") {
            let (mut rows, mut unparsed, mut pages, mut cursor) = (0, Vec::new(), 0, None);
            loop {
                let page = t3.threads(&project.id, cursor).await.expect("a thread page");
                pages += 1;
                rows += page.threads.len();
                unparsed.extend(page.unparsed);
                for thread in page.threads {
                    if busiest.as_ref().is_none_or(|b| thread.item_count > b.item_count) {
                        busiest = Some(thread);
                    }
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            println!("project {}: {pages} pages, {rows} rows parsed, {} unparsed", project.id.as_str(), unparsed.len());
            assert!(unparsed.is_empty(), "every live row parses: {unparsed:?}");
        }
        let thread = busiest.expect("some project has threads");
        let after = thread.item_count.checked_sub(u64::from(PageSize::MAX.get()) + 1).map(Position);
        let page = (t3.read(&thread.id, ReadQuery { after, limit: PageSize::MAX, max_chars: 4_000 }).await)
            .expect("the tail read");
        println!(
            "thread with {} items: read {} items with {} before them, {} unparsed, has_more {}",
            thread.item_count,
            page.items.len(),
            page.earlier,
            page.unparsed.len(),
            page.has_more
        );
        assert!(page.unparsed.is_empty(), "every live item parses: {:?}", page.unparsed);
        let window = u64::from(PageSize::MAX.get());
        assert_eq!(page.items.len() as u64, thread.item_count.min(window), "the tail window is full");
        assert_eq!(page.earlier, thread.item_count.saturating_sub(window), "it ends at the last item");
    }
}
