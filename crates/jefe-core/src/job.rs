//! A job: one goal in one repository, its tasks and its limits.

use std::collections::BTreeMap;

use acc_core::Harness;
use acc_core::harness::Launcher;
use acc_core::policy::Rule;
use acc_core::sandbox::{Preset, Sandbox};
use serde::{Deserialize, Serialize};

use crate::ids::{HostId, JobId, TaskId, WorkerId};
use crate::prompts::Prompts;
use crate::result::WorkerResult;
use crate::worker::Worker;

/// The role a task gets when its plan names none.
pub const DEFAULT_ROLE: &str = "implementor";
/// The role of the review tasks the machine adds (`[review] by`).
pub const REVIEWER_ROLE: &str = "reviewer";
/// The `jefe_role` label of the orchestrator's own session.
pub const ORCHESTRATOR_ROLE: &str = "orchestrator";

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
    /// The defaults every role inherits.
    pub workers: WorkerConfig,
    /// Roles by id. The built-in ones ([`builtin_roles`]) apply unless a
    /// role of the same id is given here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, RoleConfig>,
    /// Who decides at judgment points.
    #[serde(default)]
    pub brain: BrainSpec,
    pub budget: Budget,
    /// A human answers questions. Without one, nothing waits on a human.
    pub attended: bool,
    /// Wait for the human to approve the plan before any worker starts.
    /// `None`: when attended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approve_plan: Option<bool>,
    /// Who reviews a task's diff.
    #[serde(default)]
    pub review_by: ReviewBy,
    /// Start even with uncommitted changes in `repo`.
    pub allow_dirty: bool,
    /// Merge each task as one squashed commit.
    pub squash: bool,
    /// Diffs longer than this are cut before review.
    pub max_diff_chars: usize,
    /// A report (explorer, researcher, reviewer) longer than this is cut.
    #[serde(default = "default_max_report_chars")]
    pub max_report_chars: usize,
    pub prompts: Prompts,
}

fn default_max_report_chars() -> usize {
    20_000
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
            roles: BTreeMap::new(),
            brain: BrainSpec::default(),
            budget: Budget::default(),
            attended: false,
            approve_plan: None,
            review_by: ReviewBy::default(),
            allow_dirty: false,
            squash: false,
            max_diff_chars: 60_000,
            max_report_chars: default_max_report_chars(),
            prompts: Prompts::default(),
        }
    }

    /// The plan waits for a human's approval.
    pub fn plan_needs_approval(&self) -> bool {
        self.approve_plan.unwrap_or(self.attended)
    }

    /// The role `id`: the job's own, else a built-in one.
    pub fn role(&self, id: &str) -> Option<RoleConfig> {
        self.roles.get(id).cloned().or_else(|| builtin_roles().remove(id))
    }

    /// Every role a plan may use, built-in and configured, by id.
    pub fn all_roles(&self) -> BTreeMap<String, RoleConfig> {
        let mut all = builtin_roles();
        for (id, r) in &self.roles {
            all.insert(id.clone(), r.clone());
        }
        all
    }

    /// The roles a plan may use now.
    pub fn enabled_roles(&self) -> BTreeMap<String, RoleConfig> {
        self.all_roles().into_iter().filter(|(_, r)| r.enabled).collect()
    }

    /// Where a worker of `role` gets its settings: the role over the
    /// defaults. `None` for an unknown or disabled role.
    pub fn resolve(&self, role: &str) -> Option<ResolvedWorker> {
        let r = self.role(role)?;
        if !r.enabled {
            return None;
        }
        let w = &self.workers;
        let harness = r.harness.unwrap_or(w.harness);
        let read_only = r.access == Access::ReadOnly;
        let sandbox = match (&r.sandbox, &w.sandbox) {
            (Some(s), _) => Some(s.clone()),
            (None, Some(s)) if read_only => Some(Sandbox {
                preset: Preset::ReadOnly,
                ..s.clone()
            }),
            (None, s) => s.clone(),
        };
        let policy = if read_only {
            crate::policy::read_only_rules(&[w.policy.as_slice(), r.policy.as_slice()].concat())
        } else {
            crate::policy::worker_rules(&[w.policy.as_slice(), r.policy.as_slice()].concat())
        };
        Some(ResolvedWorker {
            role: role.to_owned(),
            harness,
            model: r.model.clone().or_else(|| w.model.clone()),
            effort: r.effort.clone().or_else(|| w.effort.clone()),
            // A read-only worker is meant to ask: the policy denies its edits.
            permission: r
                .permission
                .clone()
                .or_else(|| (!read_only).then(|| w.permission.clone()).flatten()),
            policy,
            sandbox,
            launcher: r.launcher.clone().or_else(|| w.launcher.clone()),
            alternates: r
                .alternates
                .clone()
                .unwrap_or_else(|| w.alternates.clone())
                .into_iter()
                .filter(|h| *h != harness)
                .collect(),
            unmatched: r.unmatched.unwrap_or(w.unmatched),
            access: r.access,
            max: r.max,
            prompt: r
                .prompt
                .clone()
                .or_else(|| self.prompts.roles.get(role).cloned())
                .or_else(|| crate::prompts::builtin_role_prompt(role).map(str::to_owned))
                .unwrap_or_default(),
        })
    }
}

/// How workers are started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub harness: Harness,
    pub model: Option<String>,
    /// Reasoning effort, set once the harness lists its levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
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
            effort: None,
            permission: Some("acceptEdits".to_owned()),
            alternates: vec![Harness::Codex],
            policy: Vec::new(),
            sandbox: None,
            launcher: None,
            unmatched: Unmatched::Deny,
        }
    }
}

/// What a role's worker may touch, and what it hands back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Looks around a scratch worktree under a read-only policy and hands
    /// back a report. Nothing is merged.
    ReadOnly,
    /// May edit its scratch worktree (to try things), but hands back a
    /// report. Nothing is merged.
    Report,
    /// Works in its own worktree; its diff is reviewed and merged.
    #[default]
    Worktree,
}

impl Access {
    /// The task ends with a report, not a merge.
    pub fn reports(self) -> bool {
        !matches!(self, Self::Worktree)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Report => "report",
            Self::Worktree => "worktree",
        }
    }
}

fn yes() -> bool {
    true
}

/// A role: what a kind of worker does and how it is started. Every field
/// but `access` and `enabled` falls back to [`JobSpec::workers`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleConfig {
    /// One line for the orchestrator: when to use this role.
    pub description: String,
    /// A plan may use it.
    #[serde(default = "yes")]
    pub enabled: bool,
    pub harness: Option<Harness>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission: Option<String>,
    /// Rules added to the defaults' policy.
    pub policy: Vec<Rule>,
    pub sandbox: Option<Sandbox>,
    pub access: Access,
    /// The role's instructions, rendered into the worker prompt as
    /// `{{role_prompt}}`. `None`: the built-in text for a built-in role.
    pub prompt: Option<String>,
    pub alternates: Option<Vec<Harness>>,
    pub unmatched: Option<Unmatched>,
    /// Workers of this role at once.
    pub max: Option<u32>,
    pub launcher: Option<Launcher>,
}

impl RoleConfig {
    fn builtin(description: &str, access: Access) -> Self {
        Self {
            description: description.to_owned(),
            enabled: true,
            access,
            ..Self::default()
        }
    }
}

/// The roles every job has: explorer, researcher, implementor, tester and
/// reviewer. A job's `roles` may change or add to them.
pub fn builtin_roles() -> BTreeMap<String, RoleConfig> {
    BTreeMap::from([
        (
            "explorer".to_owned(),
            RoleConfig::builtin(
                "maps the code for a question: files, entry points, risks; reports, changes nothing",
                Access::ReadOnly,
            ),
        ),
        (
            "researcher".to_owned(),
            RoleConfig::builtin(
                "looks things up (docs, the web through its tools) and reports findings; changes nothing",
                Access::ReadOnly,
            ),
        ),
        (
            DEFAULT_ROLE.to_owned(),
            RoleConfig::builtin(
                "writes code in its own worktree; its diff is reviewed and merged",
                Access::Worktree,
            ),
        ),
        (
            "tester".to_owned(),
            RoleConfig::builtin(
                "writes and runs tests in its own worktree, after the work it tests has merged; reports failures",
                Access::Worktree,
            ),
        ),
        (
            REVIEWER_ROLE.to_owned(),
            RoleConfig::builtin(
                "reads another task's diff and gives a verdict; the machine adds these, don't plan them",
                Access::ReadOnly,
            ),
        ),
    ])
}

/// A worker's settings once its role and the defaults are combined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedWorker {
    pub role: String,
    pub harness: Harness,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission: Option<String>,
    /// The whole policy, defaults included.
    pub policy: Vec<Rule>,
    pub sandbox: Option<Sandbox>,
    pub launcher: Option<Launcher>,
    pub alternates: Vec<Harness>,
    pub unmatched: Unmatched,
    pub access: Access,
    pub max: Option<u32>,
    /// The role's instructions for the worker prompt.
    pub prompt: String,
}

/// Who decides at judgment points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainKind {
    /// jefe's built-in rules (and [`crate::Effect::Consult`] for a brain
    /// the host runs).
    #[default]
    Rules,
    /// A fresh harness session for each consultation.
    Oneshot,
    /// One harness session for the whole job, consulted turn by turn, so
    /// it remembers the plan, earlier reviews and the human's notes.
    Session,
}

/// The orchestrator: a harness session the loop launches and drives itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrainSpec {
    pub kind: BrainKind,
    /// Defaults to the workers' harness.
    pub harness: Option<Harness>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission: Option<String>,
    pub launcher: Option<Launcher>,
    /// Run it in acc's read-only sandbox.
    pub sandbox: bool,
    /// Turns per session before a fresh one is seeded with the whole
    /// situation (`Session` kind).
    pub max_turns: u32,
}

impl Default for BrainSpec {
    fn default() -> Self {
        Self {
            kind: BrainKind::Rules,
            harness: None,
            model: None,
            effort: None,
            permission: None,
            launcher: None,
            sandbox: true,
            max_turns: 40,
        }
    }
}

/// Who reviews a task's diff when `review_required` is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBy {
    /// The brain alone.
    Brain,
    /// A reviewer worker's verdict decides.
    Reviewer,
    /// A reviewer worker reads the diff first; the brain decides with its
    /// verdict in hand.
    #[default]
    Both,
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
    /// Fail the job once workers and brain have spent this much (from the
    /// harnesses' own cost figures, where they give one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
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
            max_cost_usd: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Checking the repository and waiting for a plan.
    Planning,
    /// The plan waits for the human's approval.
    AwaitingApproval,
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

    pub fn name(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Running => "running",
            Self::NeedsHuman => "needs_human",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
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
    /// Read task `of`'s diff and give a verdict (`[review] by`).
    Review {
        of: TaskId,
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
    /// Finished with a report; nothing to merge (read-only and report roles).
    Done,
    Failed,
    Dropped,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Done | Self::Failed | Self::Dropped)
    }

    /// The task delivered: tasks depending on it may start.
    pub fn landed(self) -> bool {
        matches!(self, Self::Merged | Self::Done)
    }
}

fn default_role() -> String {
    DEFAULT_ROLE.to_owned()
}

/// [`DEFAULT_ROLE`] as a `String`, for serde defaults.
pub fn default_role_name() -> String {
    default_role()
}

/// What the brain plans: a task before it has an id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub title: String,
    #[serde(default)]
    pub brief: String,
    /// Titles or ids (`t1`) of tasks that must land first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Who does it (`implementor` unless said).
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl TaskSpec {
    pub fn new(title: &str, brief: &str) -> Self {
        Self {
            title: title.to_owned(),
            brief: brief.to_owned(),
            depends_on: Vec::new(),
            role: default_role(),
            harness: None,
            model: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub brief: String,
    pub kind: TaskKind,
    pub depends_on: Vec<TaskId>,
    #[serde(default = "default_role")]
    pub role: String,
    pub harness: Option<Harness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub status: TaskStatus,
    /// Workers started for it.
    pub attempts: u32,
    /// Merge conflicts it ran into.
    pub conflicts: u32,
    pub worker: Option<WorkerId>,
    pub result: Option<WorkerResult>,
    /// The diff under review, cut to `max_diff_chars`.
    pub diff: Option<String>,
    /// What a report task handed back (its final message without the
    /// result block), cut to `max_report_chars`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    pub commit: Option<String>,
    pub failure: Option<String>,
}

impl Task {
    /// The task reviewing this one, if the machine added one.
    pub fn reviews(&self) -> Option<&TaskId> {
        match &self.kind {
            TaskKind::Review { of } => Some(of),
            _ => None,
        }
    }
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
    /// What the brain's own sessions have spent.
    #[serde(default)]
    pub brain_cost_usd: f64,
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

    /// Live workers of `role`.
    pub fn live_workers_of(&self, role: &str) -> usize {
        self.workers
            .iter()
            .filter(|w| w.is_live() && w.config.role == role)
            .count()
    }

    /// What workers and brain have spent, where the harnesses say.
    pub fn cost_usd(&self) -> f64 {
        self.workers.iter().filter_map(|w| w.usage.cost_usd).sum::<f64>() + self.brain_cost_usd
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn roles_inherit_the_defaults_and_read_only_ones_cant_edit() {
        let mut spec = JobSpec::new("j", "g", "/r");
        spec.workers.model = Some("fast".to_owned());
        spec.workers.sandbox = Some(Sandbox::default());
        let imp = spec.resolve("implementor").unwrap();
        assert_eq!(imp.model.as_deref(), Some("fast"));
        assert_eq!(imp.permission.as_deref(), Some("acceptEdits"));
        assert_eq!(imp.sandbox.as_ref().map(|s| s.preset), Some(Preset::Workspace));
        assert_eq!(imp.access, Access::Worktree);
        assert!(imp.prompt.contains("implement"), "{}", imp.prompt);

        let exp = spec.resolve("explorer").unwrap();
        assert_eq!(exp.access, Access::ReadOnly);
        assert_eq!(
            exp.permission, None,
            "a read-only worker asks, and the policy denies edits"
        );
        assert_eq!(exp.sandbox.as_ref().map(|s| s.preset), Some(Preset::ReadOnly));
        assert!(
            exp.policy
                .iter()
                .any(|r| r.tool.as_deref() == Some("edit") && r.decision == acc_core::policy::Decision::Deny)
        );
    }

    #[test]
    fn a_configured_role_overrides_and_can_be_disabled() {
        let mut spec = JobSpec::new("j", "g", "/r");
        spec.roles.insert(
            "tester".to_owned(),
            RoleConfig {
                harness: Some(Harness::Pi),
                model: Some("deepseek".to_owned()),
                effort: Some("low".to_owned()),
                ..spec.role("tester").unwrap()
            },
        );
        spec.roles.insert(
            "researcher".to_owned(),
            RoleConfig {
                enabled: false,
                ..spec.role("researcher").unwrap()
            },
        );
        let t = spec.resolve("tester").unwrap();
        assert_eq!(
            (t.harness, t.model.as_deref(), t.effort.as_deref()),
            (Harness::Pi, Some("deepseek"), Some("low"))
        );
        assert_eq!(t.alternates, vec![Harness::Codex]);
        assert!(spec.resolve("researcher").is_none());
        assert!(spec.resolve("janitor").is_none());
        assert!(!spec.enabled_roles().contains_key("researcher"));
        assert!(spec.enabled_roles().contains_key("explorer"));
    }

    #[test]
    fn old_specs_still_read() {
        let json = serde_json::json!({
            "id": "j", "goal": "g", "repo": "/r", "base": null, "host": "local",
            "workers": WorkerConfig::default(), "budget": Budget::default(),
            "attended": false, "allow_dirty": false, "squash": false,
            "max_diff_chars": 1, "prompts": Prompts::default(),
        });
        let spec: JobSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.brain.kind, BrainKind::Rules);
        assert_eq!(spec.review_by, ReviewBy::Both);
        assert!(!spec.plan_needs_approval());
        let task: TaskSpec = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert_eq!(task.role, DEFAULT_ROLE);
    }
}
