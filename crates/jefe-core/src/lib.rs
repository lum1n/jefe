//! jefe-core: the sans-IO supervision loop behind jefe.
//!
//! jefe delegates a goal to coding agents through acc and supervises them.
//! This crate holds the job, task and worker state and the deterministic
//! loop ([`Jefe`]). It never does I/O: the host feeds it [`Input`]s (exec
//! results, log bytes, decisions, the clock) and runs the [`Effect`]s it
//! returns (shell scripts, log follows, brain consultations, events).
//!
//! At judgment points the loop asks a brain for one action out of a bounded
//! set; [`rules::validate`] checks the answer before anything happens.

pub mod brain;
pub mod event;
pub mod ids;
pub mod io;
mod jefe;
pub mod job;
pub mod merge;
pub mod policy;
pub mod prompts;
pub mod result;
pub mod rules;
pub mod worker;

pub use brain::{Action, ActionKind, Brain, Decision, DecisionSource, RulesBrain, Situation, Trigger};
pub use event::{Event, Stamped};
pub use ids::{AskId, ExecId, HostId, JobId, TaskId, WorkerId};
pub use io::{Effect, HumanReply, Input, Purpose};
pub use jefe::{Jefe, RESEND_AFTER_MS, RestoreError};
pub use job::{Budget, JobSpec, JobStatus, Report, TaskStatus, WorkerConfig};

/// Version of the JSON protocol jefe speaks when embedded. Consumers reject
/// majors they don't support.
pub const PROTOCOL_VERSION: u32 = 1;

/// Version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
