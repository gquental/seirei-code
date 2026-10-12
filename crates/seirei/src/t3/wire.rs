use super::{Position, T3Error, ThreadPage, TimelinePage};
use seirei_proto::{
    ids::{ItemId, ModelId, ProjectId, ProviderInstanceId, RequestId, RunId, T3Time, ThreadId, Utf16Offset},
    model::{
        Answer, Answers, Choice, Lineage, ModelSelection, PendingRequest, Project, Question, RuntimeMode, ThreadStatus,
        ThreadSummary,
    },
    timeline::{Item, ItemKind, ItemStatus, ItemText, ThreadDetail},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

pub(super) fn parse<T: DeserializeOwned>(out: Value) -> Result<T, T3Error> {
    serde_json::from_value(out).map_err(|e| T3Error::Malformed(e.to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProjectList {
    projects: Vec<ProjectRow>,
    pub(super) next_cursor: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRow {
    id: String,
    title: String,
    workspace_root: PathBuf,
    deleted_at: Option<String>,
    default_model_selection: Option<ModelSelectionRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelSelectionRow {
    instance_id: String,
    model: String,
}

impl ProjectList {
    pub(super) fn live(self) -> impl Iterator<Item = Project> {
        (self.projects.into_iter().filter(|row| row.deleted_at.is_none())).map(|row| Project {
            id: ProjectId::new(row.id),
            title: row.title,
            root: row.workspace_root,
            default_model: row.default_model_selection.map(|row| model(row.instance_id, row.model)),
        })
    }
}

fn model(provider: String, model: String) -> ModelSelection {
    ModelSelection { provider: ProviderInstanceId::new(provider), model: ModelId::new(model) }
}

#[derive(Deserialize)]
pub(super) struct Capabilities {
    providers: Vec<ProviderRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderRow {
    provider_instance_id: String,
    models: Vec<ModelRow>,
    constraints: Vec<String>,
}

#[derive(Deserialize)]
struct ModelRow {
    id: String,
}

impl Capabilities {
    /// T3 lists one constraint per reason a provider cannot run (disabled, not installed, signed
    /// out), so a provider with any constraint is skipped even when it still lists models.
    pub(super) fn runnable(self) -> impl Iterator<Item = ModelSelection> {
        (self.providers.into_iter().filter(|provider| provider.constraints.is_empty())).flat_map(|provider| {
            let id = provider.provider_instance_id;
            provider.models.into_iter().map(move |offer| model(id.clone(), offer.id))
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Launched {
    pub(super) thread_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PendingList {
    pub(super) request_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadList {
    threads: Vec<Value>,
    next_cursor: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRow {
    thread_id: String,
    title: String,
    status: ThreadStatus,
    runtime_mode: RuntimeMode,
    provider_instance_id: String,
    model: String,
    creation_source: String,
    parent_thread_id: Option<String>,
    relationship_to_parent: Option<Relationship>,
    item_count: u64,
    settled: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum Relationship {
    Fork,
    Subagent,
}

pub(super) fn thread_page(out: Value) -> Result<ThreadPage, T3Error> {
    let list: ThreadList = parse(out)?;
    let mut page = ThreadPage {
        threads: Vec::with_capacity(list.threads.len()),
        next: list.next_cursor,
        unparsed: BTreeMap::new(),
    };
    for raw in list.threads {
        let id = record_id(&raw, "threadId");
        match serde_json::from_value::<ThreadRow>(raw).map_err(|e| e.to_string()).and_then(summary) {
            Ok(thread) => page.threads.push(thread),
            Err(reason) => drop(page.unparsed.insert(ThreadId::new(id.as_str()), format!("thread {id}: {reason}"))),
        }
    }
    Ok(page)
}

fn summary(row: ThreadRow) -> Result<ThreadSummary, String> {
    let lineage = match (row.parent_thread_id, row.relationship_to_parent) {
        (None, None) => Lineage::Root { via_mcp: row.creation_source == "mcp" },
        (Some(parent), Some(Relationship::Subagent)) => Lineage::Subagent { parent: ThreadId::new(parent) },
        (Some(parent), Some(Relationship::Fork)) => Lineage::Fork { parent: ThreadId::new(parent) },
        (parent, relation) => return Err(format!("parent {parent:?} with relation {relation:?}")),
    };
    Ok(ThreadSummary {
        id: ThreadId::new(row.thread_id),
        title: row.title,
        status: row.status,
        lineage,
        runtime_mode: row.runtime_mode,
        model: model(row.provider_instance_id, row.model),
        item_count: row.item_count,
        settled: row.settled,
        created_at: t3_time(&row.created_at)?,
        updated_at: t3_time(&row.updated_at)?,
    })
}

fn record_id(raw: &Value, field: &str) -> String {
    raw.get(field).and_then(Value::as_str).unwrap_or("?").to_owned()
}

/// T3 emits JavaScript `toISOString()` (`2026-10-12T15:55:28.811Z`); anything else is malformed.
fn t3_time(raw: &str) -> Result<T3Time, String> {
    let bad = || format!("not a T3 timestamp: {raw:?}");
    let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'.'), (23, b'Z')];
    if raw.len() != 24 || separators.iter().any(|&(at, byte)| raw.as_bytes()[at] != byte) {
        return Err(bad());
    }
    let field = |from: usize, to: usize| {
        (raw.get(from..to).filter(|digits| digits.bytes().all(|b| b.is_ascii_digit())))
            .and_then(|digits| digits.parse::<i64>().ok())
            .ok_or_else(bad)
    };
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second, milli) = (field(11, 13)?, field(14, 16)?, field(17, 19)?, field(20, 23)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return Err(bad());
    }
    let seconds = ((days_from_civil(year, month, day) * 24 + hour) * 60 + minute) * 60 + second;
    Ok(T3Time(seconds * 1000 + milli))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRead {
    thread: ThreadHeader,
    items: Vec<Value>,
    has_more: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadHeader {
    #[serde(flatten)]
    row: ThreadRow,
    branch: Option<String>,
    worktree_path: Option<PathBuf>,
    active_run_id: Option<String>,
    pending_request_count: u32,
    archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemRow {
    item_id: String,
    run_id: Option<String>,
    #[serde(rename = "type")]
    kind: ItemKind,
    status: ItemStatus,
    title: Option<String>,
    text: Option<String>,
    text_truncated: bool,
    next_text_offset: Option<u64>,
    updated_at: String,
}

pub(super) fn timeline_page(out: Value, after: Option<Position>) -> Result<TimelinePage, T3Error> {
    let read: ThreadRead = parse(out)?;
    let ThreadHeader { row, branch, worktree_path, active_run_id, pending_request_count, archived } = read.thread;
    let detail = ThreadDetail {
        summary: summary(row).map_err(T3Error::Malformed)?,
        branch,
        worktree: worktree_path,
        active_run: active_run_id.map(RunId::new),
        pending_requests: pending_request_count,
        archived,
    };
    let earlier = after.map_or(0, |Position(after)| after + 1).min(detail.summary.item_count);
    let mut page = TimelinePage {
        detail,
        items: Vec::with_capacity(read.items.len()),
        earlier,
        has_more: read.has_more,
        unparsed: BTreeMap::new(),
    };
    for raw in read.items {
        let id = record_id(&raw, "itemId");
        match serde_json::from_value::<ItemRow>(raw).map_err(|e| e.to_string()).and_then(item) {
            Ok(item) => page.items.push(item),
            Err(reason) => drop(page.unparsed.insert(ItemId::new(id.as_str()), format!("item {id}: {reason}"))),
        }
    }
    Ok(page)
}

fn item(row: ItemRow) -> Result<Item, String> {
    Ok(Item {
        id: ItemId::new(row.item_id),
        kind: row.kind,
        status: row.status,
        run: row.run_id.map(RunId::new),
        title: row.title,
        text: ItemText {
            shown: row.text.unwrap_or_default(),
            more_at: row.next_text_offset.filter(|_| row.text_truncated).map(Utf16Offset),
        },
        updated_at: t3_time(&row.updated_at)?,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestRead {
    request_id: String,
    questions: Vec<QuestionRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuestionRow {
    id: String,
    header: String,
    question: String,
    options: Vec<ChoiceRow>,
    multi_select: Option<bool>,
    allow_custom_answer: Option<bool>,
    required: Option<bool>,
}

#[derive(Deserialize)]
struct ChoiceRow {
    label: String,
    description: String,
    value: Option<String>,
}

/// T3's own UI allows a custom answer unless `allowCustomAnswer` is `false`, and the respond
/// tool wants a string for every question, so both default to true.
pub(super) fn question(thread: &ThreadId, out: Value) -> Result<PendingRequest, T3Error> {
    let read: RequestRead = parse(out)?;
    let questions = (read.questions.into_iter())
        .map(|q| Question {
            id: q.id,
            header: q.header,
            prompt: q.question,
            options: (q.options.into_iter())
                .map(|c| Choice { label: c.label, description: c.description, value: c.value })
                .collect(),
            multi_select: q.multi_select.unwrap_or(false),
            allow_custom: q.allow_custom_answer.unwrap_or(true),
            required: q.required.unwrap_or(true),
        })
        .collect();
    Ok(PendingRequest { id: RequestId::new(read.request_id), thread: thread.clone(), questions })
}

/// A pick sends the choice's `value`, else its label; several picks are joined with ", ",
/// because some providers reject arrays.
pub(super) fn answers(question: &PendingRequest, answers: &Answers) -> Value {
    let by_id = (question.questions.iter()).filter_map(|q| {
        let text = match answers.0.get(&q.id)? {
            Answer::Picked(picks) => (picks.iter().filter_map(|&i| q.options.get(i)))
                .map(|choice| choice.value.as_deref().unwrap_or(&choice.label))
                .collect::<Vec<_>>()
                .join(", "),
            Answer::Custom(text) => text.trim().to_owned(),
        };
        Some((q.id.clone(), Value::String(text)))
    });
    Value::Object(by_id.collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture exists")).expect("fixture is JSON")
    }

    const COORDINATOR: &str = "7f91f114-1ab7-4948-9807-a17e97f7123f";

    #[test]
    fn timestamps_are_unix_milliseconds() {
        assert_eq!(t3_time("2026-10-12T15:55:28.811Z"), Ok(T3Time(1_791_820_528_811)));
        assert_eq!(t3_time("1970-01-01T00:00:00.000Z"), Ok(T3Time(0)));
        assert_eq!(t3_time("2024-02-29T23:59:59.999Z"), Ok(T3Time(1_709_251_199_999)));
        assert_eq!(t3_time("1969-12-31T23:59:59.999Z"), Ok(T3Time(-1)));
        for bad in
            ["2026-10-12T15:55:28Z", "2026-10-12 15:55:28.811Z", "2026-13-12T15:55:28.811Z", "+026-10-12T15:55:28.811Z"]
        {
            assert!(t3_time(bad).is_err(), "{bad} is not toISOString output");
        }
    }

    #[test]
    fn a_thread_page_keeps_every_row_it_can_read() {
        let page = thread_page(fixture("thread-list.json")).expect("the page envelope parses");
        assert_eq!(page.next, Some(6));
        let ids: Vec<&str> = page.threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids.len(), 4, "four of six rows are readable: {ids:?}");
        assert_eq!(page.unparsed.len(), 2, "an unknown status and a half-set parent fail alone: {:?}", page.unparsed);
        assert!(page.unparsed.values().any(|r| r.contains("hibernating")), "the reason names the bad value");

        let coordinator = ThreadId::new(COORDINATOR);
        let subagent = &page.threads[0];
        assert_eq!(subagent.model, model("claudeAgent".into(), "claude-opus-5-5".into()));
        assert!(subagent.id.as_str().contains("%3A"), "provider ids are kept percent-encoded");
        assert_eq!(subagent.lineage, Lineage::Subagent { parent: coordinator.clone() });
        assert_eq!(subagent.updated_at, T3Time(1_791_820_528_811));
        assert_eq!(page.threads[1].lineage, Lineage::Root { via_mcp: true });
        assert_eq!(page.threads[2].status, ThreadStatus::Waiting);
        assert_eq!(page.threads[2].runtime_mode, RuntimeMode::AutoAcceptEdits);
        assert_eq!(page.threads[3].lineage, Lineage::Fork { parent: coordinator });
        assert!(page.threads[3].settled);
    }

    #[test]
    fn a_timeline_page_maps_the_header_and_each_item() {
        let page = timeline_page(fixture("thread-read.json"), Some(Position(4))).expect("the header parses");
        assert_eq!(page.detail.summary.id.as_str(), COORDINATOR);
        assert_eq!(page.detail.summary.item_count, 105);
        assert_eq!(page.detail.pending_requests, 1);
        assert_eq!(page.detail.branch.as_deref(), Some("seirei/coordinator"));
        assert!(page.detail.active_run.is_some());
        assert_eq!(page.earlier, 5);
        assert!(page.has_more);

        assert_eq!(page.items.len(), 4, "the item with an unknown status is dropped alone");
        let odd = ItemId::new("turn-item:provider:claudeAgent:native-item:odd-01");
        assert_eq!(page.unparsed.keys().collect::<Vec<_>>(), [&odd], "unparsed items are keyed by id");
        assert!(page.unparsed[&odd].contains("odd-01"), "the reason names the item: {}", page.unparsed[&odd]);

        let message = &page.items[0];
        assert_eq!(message.kind, ItemKind::UserMessage);
        assert_eq!(message.run, None);
        assert_eq!(message.text, ItemText { shown: "Plan the cache wor".into(), more_at: Some(Utf16Offset(18)) });
        assert_eq!(page.items[1].text.more_at, None, "untruncated text has nothing more to fetch");
        assert_eq!(page.items[2].kind, ItemKind::Other("web_search".into()), "unknown kinds are carried verbatim");
        assert_eq!(page.items[2].text.shown, "", "null text reads as empty");
        assert_eq!(page.items[3].status, ItemStatus::Waiting);
    }

    #[test]
    fn a_page_whose_first_item_is_unreadable_still_counts_the_items_before_it() {
        let mut read = fixture("thread-read.json");
        read["items"][0] = json!({ "itemId": "broken" });
        let page = timeline_page(read.clone(), Some(Position(4))).expect("the header parses");
        assert_eq!((page.earlier, page.items.len(), page.unparsed.len()), (5, 3, 2));
        read["items"] = json!([]);
        let shrunk = timeline_page(read, Some(Position(200))).expect("the header parses");
        assert_eq!(shrunk.earlier, 105, "a thread shorter than the request has all its items before the page");
    }

    #[test]
    fn a_project_keeps_its_default_model_without_options() {
        let row = |default: Value| {
            json!({ "id": "p", "title": "seirei-code", "workspaceRoot": "/src", "deletedAt": null,
                    "defaultModelSelection": default })
        };
        let options = json!([{ "id": "effort", "value": "high" }]);
        let list = json!({ "projects": [
            row(json!({ "instanceId": "codex", "model": "gpt-6.1-sol", "options": options })),
            row(Value::Null),
        ], "nextCursor": null });
        let defaults: Vec<_> =
            parse::<ProjectList>(list).expect("the list parses").live().map(|p| p.default_model).collect();
        assert_eq!(defaults, [Some(model("codex".into(), "gpt-6.1-sol".into())), None]);
    }

    #[test]
    fn only_providers_without_constraints_offer_models() {
        let runnable: Vec<ModelSelection> =
            parse::<Capabilities>(fixture("capabilities.json")).expect("the capabilities parse").runnable().collect();
        let names: Vec<String> =
            runnable.iter().map(|m| format!("{}/{}", m.provider.as_str(), m.model.as_str())).collect();
        assert_eq!(
            names,
            ["codex/gpt-6.1-sol", "codex/gpt-6-astra", "claudeAgent/claude-opus-5-5"],
            "a disabled provider is skipped even when it still lists a model"
        );
    }

    #[test]
    fn a_page_without_a_readable_header_is_malformed() {
        let mut read = fixture("thread-read.json");
        read["thread"]["updatedAt"] = json!("yesterday");
        assert!(matches!(timeline_page(read, None), Err(T3Error::Malformed(_))));
    }

    #[test]
    fn questions_keep_choice_values_and_answers_send_them() {
        let thread = ThreadId::new(COORDINATOR);
        let request = question(&thread, fixture("pending-request.json")).expect("the request parses");
        assert_eq!(request.thread, thread);
        let backend = &request.questions[0];
        assert_eq!(backend.options[0].value.as_deref(), Some("memory"));
        assert_eq!(backend.options[1].value, None);
        assert!(!backend.multi_select && backend.allow_custom && backend.required, "absent flags take T3's defaults");
        let targets = &request.questions[1];
        assert!(targets.multi_select && !targets.allow_custom && !targets.required);

        let picked = Answers(
            [("backend".into(), Answer::Picked(vec![1])), ("targets".into(), Answer::Picked(vec![0, 1]))].into(),
        );
        assert_eq!(answers(&request, &picked), json!({ "backend": "SQLite", "targets": "seirei, proto" }));
        let custom = Answers([("backend".into(), Answer::Custom("  Redis \n".into()))].into());
        assert_eq!(answers(&request, &custom), json!({ "backend": "Redis" }));
    }
}
