//! A job: one goal in one repository, its tasks and its limits.

use acc_core::Harness;
use acc_core::harness::Launcher;
use acc_core::policy::Rule;
use acc_core::sandbox::Sandbox;
use serde::{Deserialize, Serialize};

use crate::ids::{HostId, JobId, TaskId, WorkerId};
use crate::prompts::Prompts;
use crate::result::WorkerResult;
use crate::worker::Worker;

/// Everything needed to start a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: JobId,
    pub goal: String,
    /// The repository on `host`, as the host's shell sees it.
    pub repo: String,
    /// The branch work merges into. It must be checked out in `repo`; `None`
    /// takes whatever is.
    pub base: Option<String>,
    pub host: HostId,
    pub workers: WorkerConfig,
    pub budget: Budget,
    /// A human answers questions. Without one, nothing waits on a human.
    pub attended: bool,
    /// Start even with uncommitted changes in `repo`.
    pub allow_dirty: bool,
    /// Merge each task as one squashed commit.
    pub squash: bool,
    /// Diffs longer than this are cut before review.
    pub max_diff_chars: usize,
    pub prompts: Prompts,
}

impl JobSpec {
    pub fn new(id: &str, goal: &str, repo: &str) -> Self {
        Self {
            id: JobId(id.to_owned()),
            goal: goal.to_owned(),
            repo: repo.to_owned(),
            base: None,
            host: HostId::local(),
            workers: WorkerConfig::default(),
            budget: Budget::default(),
            attended: false,
            allow_dirty: false,
            squash: false,
            max_diff_chars: 60_000,
            prompts: Prompts::default(),
        }
    }
}

/// How workers are started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub harness: Harness,
    pub model: Option<String>,
    /// Claude permission mode or Codex permission profile. Claude's `-p`
    /// mode needs `acceptEdits` to edit without asking.
    pub permission: Option<String>,
    /// Harnesses to hand a task to when its own one fails or runs out.
    pub alternates: Vec<Harness>,
    /// Rules added to the default worker policy.
    pub policy: Vec<Rule>,
    pub sandbox: Option<Sandbox>,
    pub launcher: Option<Launcher>,
    /// Who decides approvals no policy rule matches.
    #[serde(default)]
    pub unmatched: Unmatched,
}

/// Who decides a worker's approval that no policy rule matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unmatched {
    /// Reject it; the worker finds another way.
    #[default]
    Deny,
    /// Ask the brain (`approval` trigger: `allow` or `deny`).
    Brain,
    /// Ask the human; deny when nobody is attending.
    Human,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            harness: Harness::Claude,
            model: None,
            permission: Some("acceptEdits".to_owned()),
            alternates: vec![Harness::Codex],
            policy: Vec::new(),
            sandbox: None,
            launcher: None,
            unmatched: Unmatched::Deny,
        }
    }
}

/// Limits that keep a job bounded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// Workers running at once.
    pub max_workers: u32,
    /// Tasks in a job, conflict tasks included.
    pub max_tasks: u32,
    /// Workers per task (respawns and handoffs), and merge conflicts per task.
    pub max_attempts: u32,
    /// Prompts per worker after the first.
    pub max_turns_per_worker: u32,
    /// Brain consultations per job; after that the rules decide.
    pub max_consults: u32,
    /// A working worker silent this long is stalled.
    pub stall_ms: u64,
    /// The whole job's wall-clock limit.
    pub wall_ms: u64,
    pub brain_timeout_ms: u64,
    /// Stop a worker whose quota window is this full.
    pub quota_stop_pct: f64,
    /// Ask the brain to review each diff; otherwise accept what reports done.
    pub review_required: bool,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_workers: 3,
            max_tasks: 8,
            max_attempts: 2,
            max_turns_per_worker: 6,
            max_consults: 60,
            stall_ms: 10 * 60_000,
            wall_ms: 180 * 60_000,
            brain_timeout_ms: 300_000,
            quota_stop_pct: 90.0,
            review_required: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Checking the repository and waiting for a plan.
    Planning,
    Running,
    /// Waiting on a human answer.
    NeedsHuman,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    Implement,
    /// Merge task `of`'s branch into a fresh worktree and resolve the conflict.
    ResolveConflict {
        of: TaskId,
        branch: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    /// A worker is being prepared for it.
    Assigned,
    Working,
    /// The worker reported; a decision is due.
    Reported,
    /// Accepted, waiting for its merge.
    Accepted,
    /// Its merge conflicted; a conflict task is merging it instead.
    Conflicted,
    Merged,
    Failed,
    Dropped,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Failed | Self::Dropped)
    }
}

/// What the brain plans: a task before it has an id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub title: String,
    #[serde(default)]
    pub brief: String,
    /// Titles or ids (`t1`) of tasks that must merge first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub brief: String,
    pub kind: TaskKind,
    pub depends_on: Vec<TaskId>,
    pub harness: Option<Harness>,
    pub status: TaskStatus,
    /// Workers started for it.
    pub attempts: u32,
    /// Merge conflicts it ran into.
    pub conflicts: u32,
    pub worker: Option<WorkerId>,
    pub result: Option<WorkerResult>,
    /// The diff under review, cut to `max_diff_chars`.
    pub diff: Option<String>,
    pub commit: Option<String>,
    pub failure: Option<String>,
}

/// Counters over the whole job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub consults: u32,
    pub spawns: u32,
    pub merges: u32,
    pub conflicts: u32,
}

/// A task that landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedTask {
    pub task: TaskId,
    pub title: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub status: JobStatus,
    pub summary: String,
    pub merged: Vec<MergedTask>,
    /// Branches left for a human: work that failed, was cut short or conflicted.
    pub branches: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub goal: String,
    pub repo: String,
    pub base: Option<String>,
    pub host: HostId,
    pub status: JobStatus,
    pub tasks: Vec<Task>,
    pub workers: Vec<Worker>,
    pub counters: Counters,
    pub created_ms: u64,
    /// Messages from the human for the brain (`nudge`).
    pub notes: Vec<String>,
    pub report: Option<Report>,
}

impl Job {
    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| &t.id == id)
    }

    pub fn task_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| &t.id == id)
    }

    pub fn worker(&self, id: &WorkerId) -> Option<&Worker> {
        self.workers.iter().find(|w| &w.id == id)
    }

    pub fn worker_mut(&mut self, id: &WorkerId) -> Option<&mut Worker> {
        self.workers.iter_mut().find(|w| &w.id == id)
    }

    /// Workers holding a slot: started and not finished with.
    pub fn live_workers(&self) -> usize {
        self.workers.iter().filter(|w| w.is_live()).count()
    }

    pub fn cost_usd(&self) -> f64 {
        self.workers.iter().filter_map(|w| w.usage.cost_usd).sum()
    }
}
