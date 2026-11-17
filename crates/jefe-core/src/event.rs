//! What jefe reports: one event per thing that happened, stamped and numbered.

use acc_core::event::{Quota, Usage};
use acc_core::{BlockKind, Harness};
use serde::{Deserialize, Serialize};

use crate::brain::{Action, Decision, Trigger};
use crate::ids::{AskId, HostId, JobId, TaskId, WorkerId};
use crate::job::{JobStatus, MergedTask, TaskKind};
use crate::result::WorkerResult;

/// An event with its place in the job's log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    pub seq: u64,
    pub at_ms: u64,
    pub job: JobId,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    JobCreated {
        goal: String,
        repo: String,
        base: Option<String>,
        host: HostId,
    },
    Planned {
        tasks: Vec<TaskId>,
    },
    /// The human approved the plan; workers may start.
    PlanApproved,
    TaskAdded {
        task: TaskId,
        title: String,
        kind: TaskKind,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        depends_on: Vec<TaskId>,
        #[serde(default = "crate::job::default_role_name")]
        role: String,
    },
    TaskDropped {
        task: TaskId,
        reason: String,
    },
    TaskFailed {
        task: TaskId,
        reason: String,
    },
    /// A report task finished: its report is in the snapshot and goes to
    /// the tasks that depend on it.
    Artifact {
        task: TaskId,
        role: String,
        chars: usize,
    },
    WorkerSpawned {
        worker: WorkerId,
        task: TaskId,
        harness: Harness,
        branch: String,
        #[serde(default = "crate::job::default_role_name")]
        role: String,
    },
    WorkerPrompted {
        worker: WorkerId,
        text: String,
    },
    WorkerBlocked {
        worker: WorkerId,
        kind: BlockKind,
        title: String,
    },
    WorkerAnswered {
        worker: WorkerId,
        answers: Vec<Vec<String>>,
    },
    /// An approval no rule allowed was rejected.
    PermissionDenied {
        worker: WorkerId,
        title: String,
    },
    /// An approval no rule matched was granted by the brain or the human.
    PermissionGranted {
        worker: WorkerId,
        title: String,
    },
    WorkerReported {
        worker: WorkerId,
        task: TaskId,
        result: WorkerResult,
    },
    WorkerStalled {
        worker: WorkerId,
        idle_ms: u64,
    },
    WorkerErrored {
        worker: WorkerId,
        message: String,
    },
    WorkerExited {
        worker: WorkerId,
        code: Option<i32>,
    },
    /// `to` continues `from`'s task in the same worktree.
    WorkerReplaced {
        from: WorkerId,
        to: WorkerId,
        harness: Harness,
        handoff: bool,
    },
    Usage {
        worker: WorkerId,
        usage: Usage,
    },
    Quota {
        worker: WorkerId,
        quota: Quota,
    },
    Consulted {
        ask: AskId,
        trigger: Trigger,
    },
    Decided {
        ask: AskId,
        decision: Decision,
    },
    DecisionRejected {
        ask: AskId,
        action: Action,
        why: String,
    },
    /// The orchestrator's harness session started (`[orchestrator] kind`
    /// other than `rules`). Open it as a read-only agent view.
    BrainStarted {
        session: String,
        harness: Harness,
        model: Option<String>,
    },
    /// The orchestrator replied: `text` is what it said without its decision
    /// block, `ask` the consultation it answered.
    BrainTurn {
        session: String,
        ask: Option<AskId>,
        text: String,
    },
    /// The orchestrator's session is over (retired, exited or timed out).
    BrainEnded {
        session: String,
        why: String,
    },
    Reviewed {
        task: TaskId,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    Merged {
        task: TaskId,
        commit: String,
    },
    MergeConflict {
        task: TaskId,
        files: Vec<String>,
    },
    WorkerDiscarded {
        worker: WorkerId,
    },
    HumanAsked {
        ask: AskId,
        question: String,
        options: Vec<String>,
        /// What kind of answer: `plan` for the plan's approval.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    HumanAnswered {
        ask: AskId,
        text: String,
    },
    /// Another client took the job's lease; this one stops driving.
    LeaseLost {
        holder: String,
    },
    JobDone {
        summary: String,
        merged: Vec<MergedTask>,
        branches: Vec<String>,
    },
    JobFailed {
        reason: String,
        branches: Vec<String>,
    },
    JobCancelled {
        branches: Vec<String>,
    },
    Notice {
        message: String,
    },
}

impl Event {
    /// The job status this event ends the job with, if any.
    pub fn terminal(&self) -> Option<JobStatus> {
        match self {
            Self::JobDone { .. } => Some(JobStatus::Done),
            Self::JobFailed { .. } => Some(JobStatus::Failed),
            Self::JobCancelled { .. } => Some(JobStatus::Cancelled),
            _ => None,
        }
    }
}
