use crate::t3::Launch;
use seirei_proto::{
    ids::{ProjectId, T3Time, ThreadId},
    model::{Job, JobLink, Lineage, ModelSelection, Project, ThreadSummary},
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

const MAX_DEPTH: usize = 32;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct CoordinatorKey(String);

impl CoordinatorKey {
    pub fn mint() -> Self {
        const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut bytes = [0u8; 10];
        getrandom::fill(&mut bytes).expect("the OS random source is available");
        Self(bytes.iter().map(|b| char::from(BASE32[usize::from(b % 32)])).collect())
    }

    pub fn coordinator_tag(&self) -> String {
        format!("seirei:{}", self.0)
    }

    pub fn job_tag(&self) -> String {
        format!("job:{}", self.0)
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
pub struct Binding {
    pub key: CoordinatorKey,
    pub minted_after: T3Time,
    pub thread: Option<ThreadId>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub binding: Option<Binding>,
}

impl ProjectRecord {
    pub fn load(state_dir: &Path, project: &ProjectId) -> io::Result<Self> {
        let path = Self::path(state_dir, project);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        }
    }

    pub fn save(&self, state_dir: &Path, project: &ProjectId) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).expect("records serialize infallibly");
        crate::write_private(&Self::path(state_dir, project), &json)
    }

    pub fn saved(state_dir: &Path) -> io::Result<Vec<ProjectId>> {
        let entries = match std::fs::read_dir(state_dir.join("projects")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut projects = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                projects.push(ProjectId::new(stem));
            }
        }
        Ok(projects)
    }

    pub fn forget_all(state_dir: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(state_dir.join("projects")) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn path(state_dir: &Path, project: &ProjectId) -> PathBuf {
        state_dir.join("projects").join(format!("{}.json", project.as_str()))
    }
}

pub fn jobs(threads: &BTreeMap<ThreadId, ThreadSummary>, coordinator: &ThreadId, key: &CoordinatorKey) -> Vec<Job> {
    let mut jobs: BTreeMap<&ThreadId, Job> = (threads.values())
        .filter_map(|thread| {
            let link = job_link(thread, coordinator, key)?;
            Some((&thread.id, Job { thread: thread.clone(), link, subtasks: Vec::new() }))
        })
        .collect();
    let subtasks: Vec<(&ThreadId, &ThreadSummary)> = (threads.values())
        .filter(|thread| !jobs.contains_key(&thread.id))
        .filter_map(|thread| Some((owning_job(threads, thread, |id| jobs.contains_key(id))?, thread)))
        .collect();
    for (job, subtask) in subtasks {
        jobs.get_mut(job).expect("owning_job only names jobs").subtasks.push(subtask.clone());
    }
    let mut jobs: Vec<Job> = jobs.into_values().collect();
    for job in &mut jobs {
        job.subtasks.sort_by_key(|t| (Reverse(t.updated_at), Reverse(t.id.clone())));
    }
    jobs.sort_by_key(|job| {
        (Reverse(job.thread.status.is_live()), Reverse(job.thread.updated_at), job.thread.id.clone())
    });
    jobs
}

fn owning_job<'a>(
    threads: &'a BTreeMap<ThreadId, ThreadSummary>,
    thread: &'a ThreadSummary,
    is_job: impl Fn(&ThreadId) -> bool,
) -> Option<&'a ThreadId> {
    let mut current = thread;
    for _ in 0..MAX_DEPTH {
        let Lineage::Subagent { parent } = &current.lineage else { return None };
        if is_job(parent) {
            return Some(parent);
        }
        current = threads.get(parent)?;
    }
    None
}

fn job_link(thread: &ThreadSummary, coordinator: &ThreadId, key: &CoordinatorKey) -> Option<JobLink> {
    match &thread.lineage {
        Lineage::Subagent { parent } if parent == coordinator => Some(JobLink::Subagent),
        Lineage::Root { via_mcp: true } if thread.title.contains(&key.job_tag()) => Some(JobLink::Launched),
        _ => None,
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum EnsureStep {
    Ready(ThreadId),
    Mint,
    Lookup,
    Launch,
}

pub fn ensure_step(binding: Option<&Binding>, looked_up: bool) -> EnsureStep {
    match binding {
        None => EnsureStep::Mint,
        Some(Binding { thread: Some(thread), .. }) => EnsureStep::Ready(thread.clone()),
        Some(Binding { thread: None, .. }) if !looked_up => EnsureStep::Lookup,
        Some(Binding { thread: None, .. }) => EnsureStep::Launch,
    }
}

pub fn adopt<'a>(tagged: &'a [ThreadSummary], key: &CoordinatorKey) -> Option<&'a ThreadSummary> {
    let tag = key.coordinator_tag();
    (tagged.iter())
        .filter(|t| matches!(t.lineage, Lineage::Root { .. }) && t.title.contains(&tag))
        .min_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)))
}

pub fn launch(project: &Project, key: &CoordinatorKey, model: Option<ModelSelection>) -> Launch {
    Launch {
        project: project.id.clone(),
        title: format!("Coordinator · {} [{}]", project.title, key.coordinator_tag()),
        message: brief(project, key),
        model,
    }
}

pub fn launch_model(
    configured: Option<&ModelSelection>,
    project: &Project,
    newest: Option<&ThreadSummary>,
    runnable: &[ModelSelection],
) -> Result<Option<ModelSelection>, NoModel> {
    if let Some(configured) = configured {
        return Ok(Some(configured.clone()));
    }
    if project.default_model.is_some() {
        return Ok(None);
    }
    newest.map(|thread| &thread.model).or(runnable.first()).cloned().map(Some).ok_or(NoModel)
}

#[derive(Debug, PartialEq, thiserror::Error)]
#[error(
    "T3 has no model to run the coordinator on. Enable a provider in T3 Code, or set coordinator_model in seirei's config"
)]
pub struct NoModel;

fn brief(project: &Project, key: &CoordinatorKey) -> String {
    format!(
        "You coordinate implementation work on {title} ({root}). The user talks to you through seirei, \
         a terminal client. You plan, split work into jobs, and review results; you do not edit code yourself.\n\n\
         To start a job, call t3_thread_launch with projectId \"{id}\", \
         workspaceStrategy {{\"type\":\"worktree\",\"baseRef\":\"<base branch>\",\"branch\":\"seirei/<short-slug>\"}}, \
         a full task brief as the message, and a short imperative title that ends with \"[{job}]\". \
         Jobs without that tag in the title are invisible to the user.\n\n\
         Use delegate_task only for short read-only helpers: they share your worktree.\n\
         When you need a decision from the user, ask through a user-input request and wait for the answer.",
        job = key.job_tag(),
        title = project.title,
        root = project.root.display(),
        id = project.id.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use seirei_proto::{
        ids::{ModelId, ProviderInstanceId},
        model::{RuntimeMode, ThreadStatus},
    };

    fn model(provider: &str, model: &str) -> ModelSelection {
        ModelSelection { provider: ProviderInstanceId::new(provider), model: ModelId::new(model) }
    }

    fn thread(id: &str, title: &str, lineage: Lineage, created_at: i64) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId::new(id),
            title: title.into(),
            status: ThreadStatus::Idle,
            lineage,
            runtime_mode: RuntimeMode::FullAccess,
            model: model("claudeAgent", "claude-opus-5-5"),
            item_count: 0,
            settled: false,
            created_at: T3Time(created_at),
            updated_at: T3Time(created_at),
        }
    }

    #[test]
    fn job_links_come_from_the_parent_link_or_the_title_tag() {
        let key = CoordinatorKey("K7Q2M9XD4F".into());
        let coordinator = ThreadId::new("mcp:coord");
        let sub = thread("s", "Read the cache", Lineage::Subagent { parent: coordinator.clone() }, 1);
        let launched = thread("l", "Add the cache [job:K7Q2M9XD4F]", Lineage::Root { via_mcp: true }, 1);
        let other_key = thread("o", "Add the cache [job:OTHERKEY22]", Lineage::Root { via_mcp: true }, 1);
        let by_user = thread("u", "Add the cache [job:K7Q2M9XD4F]", Lineage::Root { via_mcp: false }, 1);
        assert_eq!(job_link(&sub, &coordinator, &key), Some(JobLink::Subagent));
        assert_eq!(job_link(&launched, &coordinator, &key), Some(JobLink::Launched));
        assert_eq!(job_link(&other_key, &coordinator, &key), None);
        assert_eq!(job_link(&by_user, &coordinator, &key), None);
    }

    #[test]
    fn ensure_never_launches_before_a_lookup() {
        let unlaunched = Binding { key: CoordinatorKey("K7Q2M9XD4F".into()), minted_after: T3Time(0), thread: None };
        assert_eq!(ensure_step(None, false), EnsureStep::Mint);
        assert_eq!(ensure_step(Some(&unlaunched), false), EnsureStep::Lookup);
        assert_eq!(ensure_step(Some(&unlaunched), true), EnsureStep::Launch);
        let bound = Binding { thread: Some(ThreadId::new("mcp:1")), ..unlaunched };
        assert_eq!(ensure_step(Some(&bound), false), EnsureStep::Ready(ThreadId::new("mcp:1")));
    }

    #[test]
    fn the_coordinator_model_falls_back_from_config_to_default_to_newest_to_runnable() {
        let configured = model("codex", "gpt-6.1-sol");
        let mut newest = thread("t", "Recent work", Lineage::Root { via_mcp: false }, 1);
        newest.model = model("grok", "grok-4.7");
        let runnable = [model("claudeAgent", "claude-opus-5-5"), model("codex", "gpt-6-astra")];
        let project =
            |default_model| Project { id: ProjectId::new("p"), title: "p".into(), root: "/p".into(), default_model };
        let bare = project(None);
        let defaulted = project(Some(model("codex", "gpt-6-luna")));

        assert_eq!(launch_model(Some(&configured), &defaulted, Some(&newest), &runnable), Ok(Some(configured)));
        assert_eq!(launch_model(None, &defaulted, Some(&newest), &runnable), Ok(None), "T3 applies the default");
        assert_eq!(launch_model(None, &bare, Some(&newest), &runnable), Ok(Some(newest.model.clone())));
        assert_eq!(launch_model(None, &bare, None, &runnable), Ok(Some(runnable[0].clone())));
        assert_eq!(launch_model(None, &bare, None, &[]), Err(NoModel));
    }

    #[test]
    fn keys_are_ten_base32_characters() {
        let key = CoordinatorKey::mint();
        assert_eq!(key.0.len(), 10);
        assert!(key.0.bytes().all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b)), "{key:?} is base32");
        assert_ne!(CoordinatorKey::mint(), key, "every coordinator gets its own key");
    }

    #[test]
    fn records_round_trip_and_a_corrupt_one_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let project = ProjectId::new("480923ae-52f3-431e-a558-42c44e1b9fb3");
        assert!(ProjectRecord::load(dir.path(), &project).expect("a missing record is fine").binding.is_none());
        let binding =
            Binding { key: CoordinatorKey::mint(), minted_after: T3Time(7), thread: Some(ThreadId::new("mcp:1")) };
        ProjectRecord { binding: Some(binding.clone()) }.save(dir.path(), &project).expect("saved");
        assert_eq!(ProjectRecord::load(dir.path(), &project).expect("readable").binding, Some(binding));
        assert_eq!(ProjectRecord::saved(dir.path()).expect("listable"), vec![project.clone()]);
        std::fs::write(dir.path().join(format!("projects/{}.json", project.as_str())), "{").expect("corrupted");
        assert!(ProjectRecord::load(dir.path(), &project).is_err(), "a corrupt record is never silently reset");
    }

    #[test]
    fn jobs_gather_subtasks_at_any_depth_and_sort_live_first() {
        let key = CoordinatorKey("K7Q2M9XD4F".into());
        let coordinator = ThreadId::new("mcp:coord");
        let sub = |parent: &str| Lineage::Subagent { parent: ThreadId::new(parent) };
        let mut launched = thread("mcp:job-a", "Add the cache [job:K7Q2M9XD4F]", Lineage::Root { via_mcp: true }, 10);
        launched.status = ThreadStatus::Running;
        let rows = [
            thread("mcp:coord", "Coordinator [seirei:K7Q2M9XD4F]", Lineage::Root { via_mcp: true }, 1),
            launched,
            thread("helper", "Read the cache", sub("mcp:job-a"), 30),
            thread("helper-of-helper", "Grep for callers", sub("helper"), 40),
            thread("sub-job", "Review the plan", sub("mcp:coord"), 50),
            thread("orphan", "Its parent is not scanned yet", sub("unseen"), 60),
            thread("cycle-a", "Broken data", sub("cycle-b"), 70),
            thread("cycle-b", "Broken data", sub("cycle-a"), 70),
            thread("mcp:other", "Another key [job:OTHERKEY22]", Lineage::Root { via_mcp: true }, 80),
        ];
        let threads: BTreeMap<ThreadId, ThreadSummary> = rows.into_iter().map(|t| (t.id.clone(), t)).collect();

        let jobs = jobs(&threads, &coordinator, &key);
        let shape: Vec<(&str, JobLink, Vec<&str>)> = (jobs.iter())
            .map(|job| (job.thread.id.as_str(), job.link, job.subtasks.iter().map(|t| t.id.as_str()).collect()))
            .collect();
        assert_eq!(
            shape,
            [
                ("mcp:job-a", JobLink::Launched, vec!["helper-of-helper", "helper"]),
                ("sub-job", JobLink::Subagent, vec![]),
            ],
            "the running job sorts above the newer idle one, and subtasks come newest first"
        );
    }

    #[test]
    fn duplicate_coordinators_converge_on_the_oldest() {
        let key = CoordinatorKey("K7Q2M9XD4F".into());
        let title = "Coordinator · seirei-code [seirei:K7Q2M9XD4F]";
        let tagged = [
            thread("b", title, Lineage::Root { via_mcp: true }, 20),
            thread("a", title, Lineage::Root { via_mcp: true }, 10),
            thread("c", "unrelated [seirei:K7Q2M9XD4F]", Lineage::Subagent { parent: ThreadId::new("a") }, 5),
        ];
        assert_eq!(adopt(&tagged, &key).map(|t| t.id.as_str()), Some("a"));
    }
}
