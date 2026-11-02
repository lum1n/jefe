//! The brain: what jefe asks at a judgment point, and what it may answer.
//!
//! The loop builds a [`Situation`] and asks for one [`Action`] out of
//! `allowed`. [`crate::rules::validate`] checks the answer before anything
//! happens. A brain never gets an executor; the closest it comes to running
//! anything is choosing an action.

pub mod rules;
pub mod wire;

use acc_core::Harness;
use acc_core::event::{Plan, Quota};
use serde::{Deserialize, Serialize};

use crate::ids::{AskId, TaskId, WorkerId};
use crate::job::{TaskKind, TaskSpec, TaskStatus};
use crate::result::WorkerResult;

pub use rules::RulesBrain;

/// Why the brain is asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// Turn the goal into tasks.
    Plan,
    /// A worker reported something other than done (blocked, needs input).
    Report { task: TaskId, worker: WorkerId },
    /// A worker reported done; its diff is ready.
    Review { task: TaskId, worker: WorkerId },
    /// A worker asked a question.
    Question { worker: WorkerId, request_id: String },
    /// A worker's turn failed.
    WorkerError { worker: WorkerId },
    /// A worker's process ended before it reported.
    Exited { worker: WorkerId },
    /// A worker's harness needs a login.
    Auth { worker: WorkerId },
    /// A worker went quiet.
    Stalled { worker: WorkerId },
    /// A worker's subscription is nearly or fully used.
    Quota { worker: WorkerId },
    /// Every task is finished: done, or more to do?
    Wrap,
}

impl Trigger {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Report { .. } => "report",
            Self::Review { .. } => "review",
            Self::Question { .. } => "question",
            Self::WorkerError { .. } => "worker_error",
            Self::Exited { .. } => "exited",
            Self::Auth { .. } => "auth",
            Self::Stalled { .. } => "stalled",
            Self::Quota { .. } => "quota",
            Self::Wrap => "wrap",
        }
    }

    pub fn worker(&self) -> Option<&WorkerId> {
        match self {
            Self::Report { worker, .. }
            | Self::Review { worker, .. }
            | Self::Question { worker, .. }
            | Self::WorkerError { worker }
            | Self::Exited { worker }
            | Self::Auth { worker }
            | Self::Stalled { worker }
            | Self::Quota { worker } => Some(worker),
            Self::Plan | Self::Wrap => None,
        }
    }
}

/// The one thing the brain asks the machine to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Replace the (empty) plan with these tasks, or add them at wrap-up.
    Plan {
        tasks: Vec<TaskSpec>,
    },
    /// Drop `task` and plan these instead.
    Split {
        task: TaskId,
        into: Vec<TaskSpec>,
    },
    /// Send a worker a message (a new turn).
    Prompt {
        worker: WorkerId,
        text: String,
    },
    /// Answer a worker's question, one list of answers per question.
    Answer {
        worker: WorkerId,
        answers: Vec<Vec<String>>,
    },
    /// Stop a worker's current turn.
    Interrupt {
        worker: WorkerId,
    },
    /// Continue a worker's task on another harness, primed with its conversation.
    Handoff {
        worker: WorkerId,
        harness: Harness,
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Start a worker's harness again, resuming its conversation.
    Respawn {
        worker: WorkerId,
    },
    /// The task's diff is good: merge it.
    Accept {
        task: TaskId,
    },
    /// Send the task back to its worker with feedback.
    Reject {
        task: TaskId,
        feedback: String,
    },
    /// Do nothing for a while.
    Wait {
        ms: u64,
    },
    AskHuman {
        question: String,
        #[serde(default)]
        options: Vec<String>,
    },
    GiveUpTask {
        task: TaskId,
        reason: String,
    },
    GiveUp {
        reason: String,
    },
    Done {
        summary: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Plan,
    Split,
    Prompt,
    Answer,
    Interrupt,
    Handoff,
    Respawn,
    Accept,
    Reject,
    Wait,
    AskHuman,
    GiveUpTask,
    GiveUp,
    Done,
}

impl ActionKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Split => "split",
            Self::Prompt => "prompt",
            Self::Answer => "answer",
            Self::Interrupt => "interrupt",
            Self::Handoff => "handoff",
            Self::Respawn => "respawn",
            Self::Accept => "accept",
            Self::Reject => "reject",
            Self::Wait => "wait",
            Self::AskHuman => "ask_human",
            Self::GiveUpTask => "give_up_task",
            Self::GiveUp => "give_up",
            Self::Done => "done",
        }
    }
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Plan { .. } => ActionKind::Plan,
            Self::Split { .. } => ActionKind::Split,
            Self::Prompt { .. } => ActionKind::Prompt,
            Self::Answer { .. } => ActionKind::Answer,
            Self::Interrupt { .. } => ActionKind::Interrupt,
            Self::Handoff { .. } => ActionKind::Handoff,
            Self::Respawn { .. } => ActionKind::Respawn,
            Self::Accept { .. } => ActionKind::Accept,
            Self::Reject { .. } => ActionKind::Reject,
            Self::Wait { .. } => ActionKind::Wait,
            Self::AskHuman { .. } => ActionKind::AskHuman,
            Self::GiveUpTask { .. } => ActionKind::GiveUpTask,
            Self::GiveUp { .. } => ActionKind::GiveUp,
            Self::Done { .. } => ActionKind::Done,
        }
    }
}

/// Who made a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum DecisionSource {
    Brain {
        harness: Option<Harness>,
        model: Option<String>,
    },
    Rules,
    Human,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    #[serde(flatten)]
    pub action: Action,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub confidence: f32,
    pub source: DecisionSource,
}

impl Decision {
    pub fn rules(action: Action, rationale: &str) -> Self {
        Self {
            action,
            rationale: rationale.to_owned(),
            confidence: 1.0,
            source: DecisionSource::Rules,
        }
    }
}

/// Anything that can decide in-process: [`RulesBrain`], a test's fake.
/// Brains that run elsewhere (a harness session) are driven by the host,
/// which answers [`crate::Effect::Consult`] with [`crate::Input::Decided`].
pub trait Brain {
    fn decide(&mut self, situation: &Situation) -> Decision;
}

// ---- the situation ----------------------------------------------------------

/// Everything the brain sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Situation {
    pub protocol: u32,
    pub ask: AskId,
    pub trigger: Trigger,
    pub goal: String,
    pub repo: String,
    /// A human can be asked.
    pub attended: bool,
    /// The default worker harness and the ones to hand off to.
    pub harness: Harness,
    pub alternates: Vec<Harness>,
    pub limits: Limits,
    pub tasks: Vec<TaskView>,
    pub workers: Vec<WorkerView>,
    pub focus: Option<Focus>,
    pub allowed: Vec<ActionKind>,
    /// Recent decisions, newest last, including rejected ones.
    pub history: Vec<DecisionView>,
    /// What the human said along the way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_workers: u32,
    pub max_tasks: u32,
    pub max_attempts: u32,
    pub max_turns_per_worker: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: TaskId,
    pub title: String,
    pub brief: String,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<TaskId>,
    pub worker: Option<WorkerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerView {
    pub id: WorkerId,
    pub task: Option<TaskId>,
    pub harness: Harness,
    pub phase: String,
    pub turns: u32,
    pub respawns: u32,
    pub stalls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    pub cost_usd: Option<f64>,
    /// The end of its last message.
    pub tail: String,
}

/// The details of what the brain is asked about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Focus {
    Review {
        task: TaskId,
        result: WorkerResult,
        diff: String,
    },
    Report {
        task: TaskId,
        result: WorkerResult,
    },
    Question {
        worker: WorkerId,
        questions: Vec<QuestionView>,
    },
    Error {
        worker: WorkerId,
        message: String,
    },
    Exit {
        worker: WorkerId,
        code: Option<i32>,
    },
    Stall {
        worker: WorkerId,
        idle_ms: u64,
    },
    Quota {
        worker: WorkerId,
        quota: Quota,
    },
    Auth {
        worker: WorkerId,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionView {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<String>,
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionView {
    pub ask: AskId,
    pub trigger: String,
    pub action: Action,
    pub source: DecisionSource,
    /// Set when the machine refused it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
}
