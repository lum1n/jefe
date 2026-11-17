//! The supervision loop: a state machine fed [`Input`]s that returns
//! [`Effect`]s. Deterministic for a given input sequence and clock, so a job
//! can be replayed from its input log.

use std::collections::{BTreeMap, BTreeSet};

use acc_core::command::Command;
use acc_core::session::{LaunchRequest, Outbound, SessionSpec, Update};
use acc_core::{BlockKind, Exec};
use serde::{Deserialize, Serialize};

use crate::brain::session::{ASK_FOR_BLOCK, BrainObs, BrainPhase, BrainSession};
use crate::brain::{
    Action, Brain, Decision, DecisionSource, DecisionView, Focus, Limits, QuestionView, ReviewView, RoleView,
    RulesBrain, Situation, TaskView, Trigger, WorkerView, wire,
};
use crate::event::{Event, Stamped};
use crate::ids::{AskId, ExecId, TaskId, WorkerId, brain_id, worker_id};
use crate::io::{Effect, HumanReply, Input, Purpose};
use crate::job::{
    Access, BrainKind, Counters, Job, JobSpec, JobStatus, MergedTask, REVIEWER_ROLE, Report, ResolvedWorker,
    ReviewBy, Task, TaskKind, TaskSpec, TaskStatus, Unmatched,
};
use crate::merge;
use crate::prompts::render;
use crate::result::{ResultStatus, WorkerResult, parse_result, strip_result};
use crate::rules::{self, Context};
use crate::worker::{Deferred, Obs, Worker, WorkerPhase};

/// Decisions kept in a situation's history.
const HISTORY: usize = 12;
/// Characters of a worker's last message shown to the brain.
const TAIL_CHARS: usize = 2_000;
/// Characters of a report shown to the brain in a task view.
const EXCERPT_CHARS: usize = 1_500;
/// Characters of the brain's reply kept in a `brain_turn` event.
const BRAIN_TEXT_CHARS: usize = 4_000;
/// Launches of the orchestrator's harness that may fail in a row before the
/// rules take over for good.
const BRAIN_LAUNCH_FAILURES: u32 = 3;

const ASK_FOR_RESULT: &str = "End your final message with the ```jefe-result block described in your instructions \
     (status done, no_change, needs_input or blocked, and a summary).";
const WRAP_UP: &str = "You were interrupted because you stopped making progress. Wrap up: commit what works and \
     report with a ```jefe-result block.";
const CONTINUE: &str = "Your session was restarted. Continue the task where you left off, and end with a \
     ```jefe-result block.";
const PLAN_KIND: &str = "plan";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PendingAsk {
    trigger: Trigger,
    focus: Option<Focus>,
    rejections: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct HumanAsk {
    ask: AskId,
    trigger: Trigger,
    /// Questions the worker asked, when the human answers for it.
    #[serde(default)]
    questions: usize,
    /// `plan` when the plan waits for approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
}

/// What the brain was last told, so a later turn can say what changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct BrainSeen {
    tasks: BTreeMap<TaskId, (TaskStatus, u32, Option<WorkerId>)>,
    workers: BTreeMap<WorkerId, String>,
    notes: usize,
}

/// The loop's own bookkeeping, saved with the job.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    seq: u64,
    next_exec: u64,
    next_ask: u32,
    next_task: u32,
    next_worker: u32,
    runs: BTreeMap<ExecId, Purpose>,
    /// What the running [`Purpose::Input`] execs write, so a restart can
    /// send what may never have arrived.
    #[serde(default)]
    inputs: BTreeMap<ExecId, Exec>,
    asks: BTreeMap<AskId, PendingAsk>,
    human: Option<HumanAsk>,
    merge_queue: Vec<TaskId>,
    merging: Option<TaskId>,
    history: Vec<DecisionView>,
    wrap_asked: bool,
    consults_exhausted: bool,
    /// The orchestrator's session, when it is a harness.
    #[serde(default)]
    brain: Option<BrainSession>,
    #[serde(default)]
    next_brain: u32,
    /// Asks waiting for the brain to finish its current turn.
    #[serde(default)]
    brain_queue: Vec<AskId>,
    #[serde(default)]
    brain_seen: BrainSeen,
    /// Launches of the brain's harness that failed in a row.
    #[serde(default)]
    brain_launch_failures: u32,
    /// The rules decide for the rest of the job: the brain's harness won't start.
    #[serde(default)]
    brain_disabled: bool,
    /// What retired brain sessions spent.
    #[serde(default)]
    brain_cost_retired: f64,
    /// Retired brain sessions whose files go once their kill is done.
    #[serde(default)]
    brain_removals: BTreeMap<String, Exec>,
    /// The job is over; waiting for cleanup execs before `Finished`.
    finishing: bool,
    finished: bool,
}

#[derive(Serialize, Deserialize)]
struct SnapshotDoc {
    v: u32,
    spec: JobSpec,
    job: serde_json::Value,
    book: serde_json::Value,
}

const SNAPSHOT_VERSION: u32 = 1;

/// After a restart, how long a worker may stay silent before what jefe may
/// not have finished writing to it is written again.
pub const RESEND_AFTER_MS: u64 = 30_000;

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("unreadable snapshot: {0}")]
    Json(#[from] serde_json::Error),
    #[error("snapshot version {0} is not supported")]
    Version(u32),
}

/// One job's supervision loop.
#[derive(Debug)]
pub struct Jefe {
    spec: JobSpec,
    job: Job,
    book: Book,
    now: u64,
    out: Vec<Effect>,
}

impl Jefe {
    /// Starts a job: checks the repository, then asks for a plan.
    pub fn new(spec: JobSpec, now_ms: u64) -> (Self, Vec<Effect>) {
        let job = Job {
            id: spec.id.clone(),
            goal: spec.goal.clone(),
            repo: spec.repo.clone(),
            base: spec.base.clone(),
            host: spec.host.clone(),
            status: JobStatus::Planning,
            tasks: Vec::new(),
            workers: Vec::new(),
            counters: Counters::default(),
            created_ms: now_ms,
            notes: Vec::new(),
            brain_cost_usd: 0.0,
            report: None,
        };
        let mut j = Self {
            spec,
            job,
            book: Book::default(),
            now: now_ms,
            out: Vec::new(),
        };
        j.emit(Event::JobCreated {
            goal: j.job.goal.clone(),
            repo: j.job.repo.clone(),
            base: j.job.base.clone(),
            host: j.job.host.clone(),
        });
        let exec = Exec::new(merge::preflight_script(&j.job.repo));
        j.run(exec, Purpose::Preflight);
        let out = std::mem::take(&mut j.out);
        (j, out)
    }

    pub fn job(&self) -> &Job {
        &self.job
    }

    pub fn spec(&self) -> &JobSpec {
        &self.spec
    }

    pub fn is_finished(&self) -> bool {
        self.book.finished
    }

    /// The orchestrator's current harness session, if it runs as one.
    pub fn brain_session(&self) -> Option<&BrainSession> {
        self.book.brain.as_ref()
    }

    /// Whether `id` names the orchestrator's session (now or earlier).
    pub fn is_brain_session(&self, id: &str) -> bool {
        id.starts_with(&format!("jefe-{}-b", self.job.id))
    }

    /// When the loop next needs a [`Input::Tick`]: the earliest stall,
    /// held-back prompt, brain timeout or wall-clock deadline.
    pub fn next_deadline(&self) -> Option<u64> {
        if self.job.status.is_terminal() {
            return None;
        }
        let b = &self.spec.budget;
        let wall = self.job.created_ms.saturating_add(b.wall_ms);
        let brain = self.book.brain.iter().flat_map(|br| {
            let timeout = match &br.phase {
                BrainPhase::Thinking { since_ms, .. } if br.deferred.is_none() => {
                    Some(since_ms.saturating_add(b.brain_timeout_ms))
                }
                _ => None,
            };
            timeout
                .into_iter()
                .chain(br.deferred.as_ref().map(Deferred::deadline))
        });
        self.job
            .workers
            .iter()
            .filter(|w| matches!(w.phase, WorkerPhase::Working | WorkerPhase::Waiting))
            .map(|w| w.last_progress_ms.saturating_add(b.stall_ms))
            .chain(self.job.workers.iter().filter_map(|w| w.resend_at))
            .chain(
                self.job
                    .workers
                    .iter()
                    .filter_map(|w| w.deferred.as_ref().map(Deferred::deadline)),
            )
            .chain(brain)
            .chain([wall])
            .min()
    }

    /// Advances the loop by one input.
    pub fn step(&mut self, input: Input, now_ms: u64) -> Vec<Effect> {
        self.now = now_ms;
        if self.book.finished {
            return Vec::new();
        }
        match input {
            Input::Tick => self.on_tick(),
            Input::ExecDone {
                id,
                code,
                stdout,
                stderr,
            } => self.on_exec(id, code, &stdout, &stderr),
            Input::Bytes { worker, bytes } => {
                if self.is_brain_session(worker.as_str()) {
                    self.on_brain_bytes(worker.as_str(), &bytes);
                } else {
                    let updates = match self.job.worker_mut(&worker) {
                        Some(w) => w.feed(&bytes, now_ms),
                        None => Vec::new(),
                    };
                    self.on_updates(&worker, &updates);
                }
            }
            Input::LogEnded { worker, code } => {
                if self.is_brain_session(worker.as_str()) {
                    self.on_brain_log_ended(worker.as_str(), code);
                } else {
                    let updates = match self.job.worker_mut(&worker) {
                        Some(w) => w.on_exit(code, now_ms),
                        None => Vec::new(),
                    };
                    self.on_updates(&worker, &updates);
                    if let Some(w) = self.job.worker(&worker)
                        && !matches!(
                            w.phase,
                            WorkerPhase::Exited { .. } | WorkerPhase::Gone | WorkerPhase::Discarding
                        )
                        && w.replaced_by.is_none()
                    {
                        self.on_exited(&worker, code);
                    }
                }
            }
            Input::Decided { ask, decision } => self.on_decided(ask, decision),
            Input::BrainFailed { ask, why } => self.brain_failed(&ask, &why),
            Input::Human(reply) => self.on_human(reply),
        }
        self.settle();
        std::mem::take(&mut self.out)
    }

    /// Records an event from outside the loop (the host's state store, say)
    /// with the job's own numbering.
    pub fn record(&mut self, event: Event, now_ms: u64) -> Vec<Effect> {
        self.now = now_ms;
        self.emit(event);
        std::mem::take(&mut self.out)
    }

    // ---- snapshot ------------------------------------------------------------

    /// Everything needed to pick the job up after a restart, as JSON.
    pub fn snapshot(&self) -> String {
        let mut job = serde_json::to_value(&self.job).unwrap_or_default();
        if let Some(ws) = job.get_mut("workers").and_then(|w| w.as_array_mut()) {
            for (v, w) in ws.iter_mut().zip(&self.job.workers) {
                if let Some(snap) = w.snapshot() {
                    v["session_snapshot"] = snap.into();
                }
            }
        }
        let mut book = serde_json::to_value(&self.book).unwrap_or_default();
        if let Some(snap) = self.book.brain.as_ref().and_then(BrainSession::snapshot)
            && let Some(b) = book.get_mut("brain")
        {
            b["session_snapshot"] = snap.into();
        }
        let doc = serde_json::json!({
            "v": SNAPSHOT_VERSION,
            "spec": &self.spec,
            "job": job,
            "book": book,
        });
        doc.to_string()
    }

    /// Picks a job up from [`Jefe::snapshot`]. `specs` are the acc sessions
    /// the host lists now (`acc_core::detached::list_script`). Execs that
    /// were running when the snapshot was taken are run again where that is
    /// safe; workers whose session is gone count as exited.
    pub fn restore(
        json: &str,
        specs: &[SessionSpec],
        now_ms: u64,
    ) -> Result<(Self, Vec<Effect>), RestoreError> {
        let doc: SnapshotDoc = serde_json::from_str(json)?;
        if doc.v != SNAPSHOT_VERSION {
            return Err(RestoreError::Version(doc.v));
        }
        let job: Job = serde_json::from_value(doc.job)?;
        let book: Book = serde_json::from_value(doc.book)?;
        let lost: Vec<(ExecId, Purpose)> = book.runs.clone().into_iter().collect();
        let mut j = Self {
            spec: doc.spec,
            job,
            book,
            now: now_ms,
            out: Vec::new(),
        };
        j.book.runs.clear();
        let mut lost_inputs = std::mem::take(&mut j.book.inputs);
        if j.book.finished {
            return Ok((j, Vec::new()));
        }
        let mut gone = Vec::new();
        for w in &mut j.job.workers {
            if w.discarded || (matches!(w.phase, WorkerPhase::Gone) && w.cwd.is_none()) {
                continue;
            }
            match specs.iter().find(|s| s.id == w.id.as_str()) {
                Some(spec) => {
                    if w.reattach(spec.clone(), w.config.policy.clone()).is_err() {
                        gone.push(w.id.clone());
                    }
                }
                None if w.phase == WorkerPhase::Preparing || w.pending_launch.is_some() => {}
                None => gone.push(w.id.clone()),
            }
        }
        // The brain: back to its session when it still runs, else a fresh
        // one answers the asks it was thinking about.
        let mut brain_waits_on: Option<AskId> = None;
        if let Some(brain) = &mut j.book.brain
            && brain.is_live()
        {
            match specs.iter().find(|s| s.id == brain.id) {
                Some(spec) if brain.reattach(spec.clone()).is_ok() => {
                    brain_waits_on = brain.thinking_about().cloned();
                }
                _ => {
                    brain.phase = BrainPhase::Gone;
                }
            }
        }
        j.book.brain_queue.clear();
        // Re-run what was in flight where running it twice is harmless.
        for (id, p) in lost {
            match p {
                Purpose::Preflight => j.run(
                    Exec::new(merge::preflight_script(&j.job.repo)),
                    Purpose::Preflight,
                ),
                Purpose::WorktreeAdd(w) => j.add_worktree(&w),
                Purpose::Diff(t) => j.run_diff(&t),
                Purpose::Merge(t) => {
                    j.book.merging = None;
                    j.book.merge_queue.insert(0, t);
                }
                Purpose::ConflictFiles(t) => j.run_conflict_files(&t),
                Purpose::Discard(w) => j.discard(&w),
                Purpose::Kill(w) => j.kill(&w),
                Purpose::Remove(w) => j.remove(&w),
                // Maybe written, maybe not: send it again unless the worker
                // shows it got it (see `Worker::unsent`).
                Purpose::Input(w) => {
                    let resend_at = j.now.saturating_add(RESEND_AFTER_MS);
                    if let (Some(exec), Some(worker)) = (lost_inputs.remove(&id), j.job.worker_mut(&w)) {
                        worker.unsent.push(exec);
                        worker.resend_at = Some(resend_at);
                    }
                }
                Purpose::BrainKill(id) => j.brain_remove(&id),
                Purpose::BrainRemove(_) => {}
                Purpose::Launch(_)
                | Purpose::Status(_)
                | Purpose::SpecWrite(_)
                | Purpose::BrainLaunch(_)
                | Purpose::BrainInput(_)
                | Purpose::BrainStatus(_)
                | Purpose::BrainSpecWrite(_)
                | Purpose::Save
                | Purpose::Lease
                | Purpose::Release => {}
            }
        }
        // A replacement waiting for its predecessor's kill: launch it now
        // unless a re-run kill will.
        let waiting: Vec<WorkerId> = j
            .job
            .workers
            .iter()
            .filter(|w| w.pending_launch.is_some())
            .map(|w| w.id.clone())
            .collect();
        for next in waiting {
            let killing = j.job.workers.iter().any(|old| {
                old.replaced_by.as_ref() == Some(&next)
                    && j.book.runs.values().any(|p| *p == Purpose::Kill(old.id.clone()))
            });
            if !killing {
                j.run_pending_launch(&next);
            }
        }
        let live: Vec<WorkerId> = j
            .job
            .workers
            .iter()
            .filter(|w| {
                w.has_session()
                    && w.is_live()
                    && !j.book.runs.values().any(|p| *p == Purpose::Launch(w.id.clone()))
            })
            .map(|w| w.id.clone())
            .collect();
        for id in live {
            if let Some(exec) = j.job.worker(&id).and_then(Worker::status_exec) {
                j.run(exec, Purpose::Status(id));
            }
        }
        if let Some(brain) = &j.book.brain
            && brain.is_live()
            && brain.has_session()
            && let Some(exec) = brain.status_exec()
        {
            let id = brain.id.clone();
            j.run(exec, Purpose::BrainStatus(id));
        }
        for id in gone {
            if let Some(w) = j.job.worker_mut(&id)
                && w.is_live()
            {
                j.on_exited(&id, None);
            }
        }
        let asks: Vec<(AskId, PendingAsk)> = std::mem::take(&mut j.book.asks).into_iter().collect();
        for (ask, pending) in asks {
            if brain_waits_on.as_ref() == Some(&ask) {
                // Its turn still runs; the reply comes with the log.
                j.book.asks.insert(ask, pending);
            } else {
                j.issue_consult(ask, pending);
            }
        }
        if let Some(h) = j.book.human.clone() {
            j.out.push(Effect::AskHuman {
                ask: h.ask,
                question: "(asked before the restart; see the log)".to_owned(),
                options: Vec::new(),
                kind: h.kind,
            });
        }
        j.notice("resumed".to_owned());
        j.settle();
        let out = std::mem::take(&mut j.out);
        Ok((j, out))
    }

    // ---- plumbing ------------------------------------------------------------

    fn cx(&self) -> Context<'_> {
        Context { spec: &self.spec }
    }

    fn emit(&mut self, event: Event) {
        self.book.seq += 1;
        self.out.push(Effect::Emit(Stamped {
            seq: self.book.seq,
            at_ms: self.now,
            job: self.job.id.clone(),
            event,
        }));
    }

    fn notice(&mut self, message: String) {
        self.emit(Event::Notice { message });
    }

    fn run(&mut self, exec: Exec, purpose: Purpose) {
        self.book.next_exec += 1;
        let id = ExecId(self.book.next_exec);
        self.book.runs.insert(id, purpose.clone());
        if matches!(purpose, Purpose::Input(_)) {
            self.book.inputs.insert(id, exec.clone());
        }
        self.out.push(Effect::Run {
            id,
            host: self.job.host.clone(),
            exec,
            purpose,
        });
    }

    fn next_ask(&mut self) -> AskId {
        self.book.next_ask += 1;
        AskId(format!("a{}", self.book.next_ask))
    }

    /// After every step: start what can start, wrap up, finish.
    fn settle(&mut self) {
        if self.job.status == JobStatus::Running {
            self.dispatch();
            self.next_merge();
            self.maybe_wrap();
        }
        self.check_cost();
        if self.book.finishing && self.book.runs.is_empty() && !self.book.finished {
            self.book.finished = true;
            let report = self
                .job
                .report
                .clone()
                .unwrap_or_else(|| self.report(String::new()));
            self.out.push(Effect::Finished {
                status: self.job.status,
                report,
            });
        }
    }

    /// Ends the job once it has spent more than its cap.
    fn check_cost(&mut self) {
        if let Some(cap) = self.spec.budget.max_cost_usd
            && !self.job.status.is_terminal()
        {
            let spent = self.job.cost_usd();
            if spent > cap {
                self.fail_job(format!("the job spent ${spent:.2}, over its ${cap:.2} cap"));
            }
        }
    }

    // ---- execs -----------------------------------------------------------------

    fn on_exec(&mut self, id: ExecId, code: i32, stdout: &str, stderr: &str) {
        let Some(purpose) = self.book.runs.remove(&id) else {
            return;
        };
        self.book.inputs.remove(&id);
        let err = || {
            let e = stderr.trim();
            if e.is_empty() {
                format!("exit {code}")
            } else {
                e.to_owned()
            }
        };
        match purpose {
            Purpose::Preflight => self.on_preflight(code, stdout, &err()),
            Purpose::WorktreeAdd(w) => {
                if code != 0 {
                    self.start_failed(&w, format!("making the worktree failed: {}", err()));
                } else {
                    let cwd = stdout
                        .lines()
                        .map(str::trim)
                        .rfind(|l| !l.is_empty())
                        .unwrap_or_default();
                    self.launch(&w, cwd.to_owned());
                }
            }
            Purpose::Launch(w) => {
                if code != 0 {
                    self.start_failed(&w, format!("starting the harness failed: {}", err()));
                    return;
                }
                let Some(worker) = self.job.worker_mut(&w) else {
                    return;
                };
                let updates = worker.on_launched(stdout);
                worker.phase = WorkerPhase::Working;
                worker.last_progress_ms = self.now;
                let host = worker.host.clone();
                self.out.push(Effect::Follow {
                    worker: w.clone(),
                    host,
                    offset: 0,
                });
                self.on_updates(&w, &updates);
            }
            Purpose::Input(w) => {
                if code != 0 {
                    self.notice(format!("writing to {w} failed: {}", err()));
                }
            }
            Purpose::SpecWrite(_) | Purpose::Remove(_) => {}
            Purpose::Status(w) => {
                let Some(worker) = self.job.worker_mut(&w) else {
                    return;
                };
                let updates = worker.on_status(stdout, self.now);
                let (host, offset) = (worker.host.clone(), worker.offset());
                self.out.push(Effect::Follow {
                    worker: w.clone(),
                    host,
                    offset,
                });
                self.on_updates(&w, &updates);
            }
            Purpose::Diff(t) => self.on_diff(&t, code, stdout, &err()),
            Purpose::Merge(t) => self.on_merge(&t, code, stdout, &err()),
            Purpose::ConflictFiles(t) => {
                let files: Vec<String> = stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect();
                self.on_conflict_files(&t, files);
            }
            Purpose::Discard(w) => {
                if let Some(worker) = self.job.worker_mut(&w) {
                    worker.phase = WorkerPhase::Gone;
                    worker.discarded = code == 0;
                }
                if code == 0 {
                    self.emit(Event::WorkerDiscarded { worker: w.clone() });
                } else {
                    self.notice(format!("deleting {w}'s worktree failed: {}", err()));
                }
                self.remove(&w);
            }
            Purpose::Kill(w) => {
                // A replacement waits for its predecessor to stop.
                let next = self
                    .job
                    .worker(&w)
                    .and_then(|old| old.replaced_by.clone())
                    .filter(|n| self.job.worker(n).is_some_and(|nw| nw.pending_launch.is_some()));
                if let Some(next) = next {
                    self.run_pending_launch(&next);
                }
            }
            Purpose::BrainLaunch(id) => self.on_brain_launched(&id, code, stdout, &err()),
            Purpose::BrainStatus(id) => {
                let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id) else {
                    return;
                };
                let updates = brain.on_status(stdout, self.now);
                let offset = brain.offset();
                self.out.push(Effect::Follow {
                    worker: WorkerId(id.clone()),
                    host: self.job.host.clone(),
                    offset,
                });
                self.on_brain_updates(&id, &updates);
            }
            Purpose::BrainInput(id) => {
                if code != 0 {
                    self.notice(format!("writing to the orchestrator ({id}) failed: {}", err()));
                }
            }
            Purpose::BrainKill(id) => self.brain_remove(&id),
            Purpose::BrainSpecWrite(_)
            | Purpose::BrainRemove(_)
            | Purpose::Save
            | Purpose::Lease
            | Purpose::Release => {}
        }
    }

    fn on_preflight(&mut self, code: i32, stdout: &str, err: &str) {
        let pf = if code == 0 {
            merge::parse_preflight(stdout)
        } else {
            None
        };
        let Some(pf) = pf else {
            self.fail_job(format!("{} is not a git repository: {err}", self.job.repo));
            return;
        };
        self.job.repo = pf.top;
        match &self.job.base {
            Some(base) if *base != pf.branch => {
                let on = if pf.branch.is_empty() {
                    "a detached HEAD"
                } else {
                    pf.branch.as_str()
                };
                self.fail_job(format!(
                    "{base} must be checked out in {} (it is on {on})",
                    self.job.repo
                ));
                return;
            }
            Some(_) => {}
            None if pf.branch.is_empty() => {
                self.fail_job(format!(
                    "{} is on a detached HEAD; check out a branch to merge into",
                    self.job.repo
                ));
                return;
            }
            None => self.job.base = Some(pf.branch),
        }
        if pf.dirty && !self.spec.allow_dirty {
            self.fail_job(format!(
                "{} has uncommitted changes; commit or stash them first (or allow a dirty checkout)",
                self.job.repo
            ));
            return;
        }
        self.consult(Trigger::Plan, None);
    }

    // ---- workers -----------------------------------------------------------------

    /// Starts workers for pending tasks while slots are free.
    fn dispatch(&mut self) {
        let pending: Vec<TaskId> = self
            .job
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .map(|t| t.id.clone())
            .collect();
        for id in pending {
            let Some(t) = self.job.task(&id) else { continue };
            let deps: Vec<TaskStatus> = t
                .depends_on
                .iter()
                .filter_map(|d| self.job.task(d))
                .map(|d| d.status)
                .collect();
            let review = matches!(t.kind, TaskKind::Review { .. });
            let role = t.role.clone();
            if deps
                .iter()
                .any(|s| matches!(s, TaskStatus::Failed | TaskStatus::Dropped))
            {
                self.drop_task(&id, "a task it depends on did not land".to_owned());
                continue;
            }
            if !deps.iter().all(|s| s.landed()) {
                continue;
            }
            // A review is short and read-only: it may run beside a full
            // house, else a lone reported worker would block its own review.
            if !review && self.job.live_workers() >= self.spec.budget.max_workers as usize {
                break;
            }
            let Some(cfg) = self.spec.resolve(&role) else {
                self.fail_task(&id, format!("its role {role:?} is not available"));
                continue;
            };
            if cfg
                .max
                .is_some_and(|m| self.job.live_workers_of(&role) >= m as usize)
            {
                continue;
            }
            self.spawn(&id, cfg);
        }
    }

    fn new_worker_id(&mut self) -> WorkerId {
        self.book.next_worker += 1;
        worker_id(&self.job.id, self.book.next_worker)
    }

    fn spawn(&mut self, task: &TaskId, mut cfg: ResolvedWorker) {
        let id = self.new_worker_id();
        let Some(t) = self.job.task_mut(task) else { return };
        if let Some(h) = t.harness {
            cfg.harness = h;
            cfg.alternates.retain(|a| *a != h);
        }
        if let Some(m) = &t.model {
            cfg.model = Some(m.clone());
        }
        t.status = TaskStatus::Assigned;
        t.worker = Some(id.clone());
        t.attempts += 1;
        let worker = Worker::new(
            id.clone(),
            task.clone(),
            cfg,
            self.job.host.clone(),
            id.0.clone(),
            self.now,
        );
        self.job.workers.push(worker);
        self.job.counters.spawns += 1;
        self.add_worktree(&id);
    }

    fn add_worktree(&mut self, w: &WorkerId) {
        let Some(worker) = self.job.worker(w) else { return };
        // From the base branch's tip, so tasks see what their dependencies merged.
        let script = acc_core::worktree::add_script(&self.job.repo, &worker.worktree, None);
        self.run(Exec::new(script), Purpose::WorktreeAdd(w.clone()));
    }

    /// The reports and diffs a task's brief is completed with: what the
    /// tasks it depends on handed back, and the diff it reviews.
    fn task_inputs(&self, task: &Task) -> String {
        let mut inputs = String::new();
        if let TaskKind::Review { of } = &task.kind
            && let Some(t) = self.job.task(of)
        {
            inputs.push_str(&format!(
                "## The task under review: {} ({})\n\n{}\n\n",
                t.id, t.title, t.brief
            ));
            if let Some(r) = &t.result {
                inputs.push_str(&format!("The worker's summary: {}\n\n", r.summary));
            }
            inputs.push_str(&format!(
                "## Diff of {}\n\n```diff\n{}\n```\n\n",
                t.id,
                t.diff.as_deref().unwrap_or("(empty)")
            ));
        }
        for dep in &task.depends_on {
            if let Some(t) = self.job.task(dep)
                && let Some(report) = &t.report
            {
                inputs.push_str(&format!(
                    "## Report from {} ({}): {}\n\n{}\n\n",
                    t.id, t.role, t.title, report
                ));
            }
        }
        if inputs.is_empty() {
            inputs
        } else {
            format!("What you were given:\n\n{inputs}")
        }
    }

    fn worker_prompt(
        &self,
        task: &Task,
        cfg: &ResolvedWorker,
        branch: &str,
        feedback: Option<&str>,
    ) -> String {
        let feedback = feedback
            .map(|f| format!("\nFeedback on your previous attempt:\n\n{f}\n"))
            .unwrap_or_default();
        let vars = BTreeMap::from([
            ("goal", self.job.goal.clone()),
            ("task_title", task.title.clone()),
            ("task_brief", task.brief.clone()),
            ("role", cfg.role.clone()),
            ("role_prompt", cfg.prompt.trim().to_owned()),
            ("inputs", self.task_inputs(task)),
            ("branch", branch.to_owned()),
            ("feedback", feedback),
        ]);
        render(&self.spec.prompts.worker, &vars)
    }

    fn launch(&mut self, w: &WorkerId, cwd: String) {
        let Some(worker) = self.job.worker(w) else { return };
        let Some(task) = worker.task.as_ref().and_then(|t| self.job.task(t)) else {
            return;
        };
        let cfg = worker.config.clone();
        let prompt = self.worker_prompt(task, &cfg, &worker.branch, None);
        let deferred = cfg
            .effort
            .clone()
            .map(|effort| Deferred::new(prompt.clone(), effort, self.now));
        let req = LaunchRequest {
            harness: Some(worker.harness),
            cwd: cwd.clone(),
            model: cfg.model.clone(),
            permission: cfg.permission.clone(),
            title: Some(task.title.clone()),
            prompt: deferred.is_none().then_some(prompt),
            labels: BTreeMap::from([
                ("jefe_job".to_owned(), self.job.id.0.clone()),
                ("jefe_task".to_owned(), task.id.0.clone()),
                ("jefe_role".to_owned(), cfg.role.clone()),
            ]),
            launcher: cfg.launcher.clone(),
            worktree: Some(worker.worktree.clone()),
            sandbox: cfg.sandbox.clone(),
            ..LaunchRequest::default()
        };
        let task_id = task.id.clone();
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        worker.cwd = Some(cwd);
        worker.deferred = deferred;
        match worker.launch(req, cfg.policy.clone(), now) {
            Ok(exec) => {
                worker.phase = WorkerPhase::Launching;
                let (harness, branch) = (worker.harness, worker.branch.clone());
                if let Some(t) = self.job.task_mut(&task_id) {
                    t.status = TaskStatus::Working;
                }
                self.emit(Event::WorkerSpawned {
                    worker: w.clone(),
                    task: task_id,
                    harness,
                    branch,
                    role: cfg.role,
                });
                self.run(exec, Purpose::Launch(w.clone()));
            }
            Err(e) => self.start_failed(w, format!("can't launch: {e}")),
        }
    }

    /// A worker never got going: count it as an exit, which the brain handles.
    fn start_failed(&mut self, w: &WorkerId, message: String) {
        self.emit(Event::WorkerErrored {
            worker: w.clone(),
            message: message.clone(),
        });
        let has_session = self.job.worker(w).is_some_and(Worker::has_session);
        if let Some(worker) = self.job.worker_mut(w) {
            worker.phase = WorkerPhase::Gone;
        }
        let task = self.job.worker(w).and_then(|x| x.task.clone());
        if let Some(task) = task {
            if has_session {
                if let Some(worker) = self.job.worker_mut(w) {
                    worker.phase = WorkerPhase::Errored { message };
                }
                self.consult(
                    Trigger::Exited { worker: w.clone() },
                    Some(Focus::Exit {
                        worker: w.clone(),
                        code: None,
                    }),
                );
            } else {
                self.fail_task(&task, message);
            }
        }
    }

    fn flush(&mut self, w: &WorkerId) {
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let (input, spec) = worker.flush();
        if let Some(exec) = input {
            self.run(exec, Purpose::Input(w.clone()));
        }
        if let Some(exec) = spec {
            self.run(exec, Purpose::SpecWrite(w.clone()));
        }
    }

    /// Sends a worker's held-back first prompt once its harness has listed
    /// its effort levels (or the wait is over).
    fn send_deferred(&mut self, w: &WorkerId) {
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let Some(cmds) = worker.deferred_due(now) else {
            return;
        };
        worker.deferred = None;
        let mut execs = Vec::new();
        for cmd in cmds {
            if let Ok(Outbound::Send(bytes)) = worker.command(cmd)
                && let Some(exec) = worker.input_exec(bytes)
            {
                execs.push(exec);
            }
        }
        worker.last_progress_ms = now;
        for exec in execs {
            self.run(exec, Purpose::Input(w.clone()));
        }
    }

    fn on_updates(&mut self, w: &WorkerId, updates: &[Update]) {
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let was_replaying = updates.iter().any(|u| u.replay);
        if updates.iter().any(|u| !u.replay) {
            // It is talking, so what was written before the restart arrived.
            worker.unsent.clear();
            worker.resend_at = None;
        }
        let obs = worker.observe(updates);
        let caught_up = was_replaying && !worker.is_replaying();
        for o in obs {
            self.on_obs(w, o);
        }
        if caught_up {
            self.reconcile(w);
        }
        self.on_requests(w);
        self.send_deferred(w);
        self.flush(w);
    }

    /// After replaying a log on restore: act on how the session stands now,
    /// since what happened while jefe was away arrived as history.
    fn reconcile(&mut self, w: &WorkerId) {
        let Some(worker) = self.job.worker(w) else { return };
        if !matches!(
            worker.phase,
            WorkerPhase::Working | WorkerPhase::Launching | WorkerPhase::Waiting
        ) {
            return;
        }
        if !worker.is_alive() {
            self.on_exited(w, None);
            return;
        }
        if let Some(acc_core::RunState::Done | acc_core::RunState::Idle | acc_core::RunState::Error) =
            worker.state()
        {
            let end = worker.turn_end();
            self.on_obs(w, end);
        }
    }

    fn on_obs(&mut self, w: &WorkerId, o: Obs) {
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        if !worker.is_live() {
            return;
        }
        match o {
            Obs::Progress => {
                worker.last_progress_ms = now;
                if matches!(worker.phase, WorkerPhase::Stalled { .. } | WorkerPhase::Launching) {
                    worker.phase = WorkerPhase::Working;
                }
            }
            Obs::TurnWaiting => {
                worker.phase = WorkerPhase::Waiting;
                worker.last_progress_ms = now;
            }
            Obs::TurnDone => {
                // A turn before the first prompt (a harness greeting) is nothing.
                if worker.deferred.is_none() {
                    self.on_turn_done(w);
                }
            }
            Obs::TurnError(message) => {
                worker.phase = WorkerPhase::Errored {
                    message: message.clone(),
                };
                self.emit(Event::WorkerErrored {
                    worker: w.clone(),
                    message: message.clone(),
                });
                self.consult(
                    Trigger::WorkerError { worker: w.clone() },
                    Some(Focus::Error {
                        worker: w.clone(),
                        message,
                    }),
                );
            }
            Obs::Auth(message) => {
                if worker.handled.iter().any(|h| h == "auth") {
                    return;
                }
                worker.handled.push("auth".to_owned());
                worker.phase = WorkerPhase::Blocked {
                    kind: BlockKind::Auth,
                    request_id: String::new(),
                };
                self.emit(Event::WorkerBlocked {
                    worker: w.clone(),
                    kind: BlockKind::Auth,
                    title: message.clone(),
                });
                self.consult(
                    Trigger::Auth { worker: w.clone() },
                    Some(Focus::Auth {
                        worker: w.clone(),
                        message,
                    }),
                );
            }
            Obs::Quota(quota) => {
                let limit = self.spec.budget.quota_stop_pct;
                let out = quota.windows.iter().any(|q| {
                    q.status.as_deref() == Some("rejected") || q.used_percent.is_some_and(|p| p >= limit)
                });
                let first = !worker.handled.iter().any(|h| h == "quota");
                if out && first && matches!(worker.phase, WorkerPhase::Working | WorkerPhase::Waiting) {
                    worker.handled.push("quota".to_owned());
                    self.emit(Event::Quota {
                        worker: w.clone(),
                        quota: quota.clone(),
                    });
                    self.consult(
                        Trigger::Quota { worker: w.clone() },
                        Some(Focus::Quota {
                            worker: w.clone(),
                            quota,
                        }),
                    );
                }
            }
            Obs::Exited(code) => self.on_exited(w, code),
            Obs::Notice(message) => self.notice(format!("{w}: {message}")),
        }
    }

    fn on_exited(&mut self, w: &WorkerId, code: Option<i32>) {
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        if matches!(
            worker.phase,
            WorkerPhase::Exited { .. } | WorkerPhase::Gone | WorkerPhase::Discarding
        ) || worker.replaced_by.is_some()
        {
            return;
        }
        let was = std::mem::replace(&mut worker.phase, WorkerPhase::Exited { code });
        self.out.push(Effect::Unfollow { worker: w.clone() });
        self.emit(Event::WorkerExited {
            worker: w.clone(),
            code,
        });
        let task = self.job.worker(w).and_then(|x| x.task.clone());
        let active = task
            .as_ref()
            .and_then(|t| self.job.task(t))
            .is_some_and(|t| matches!(t.status, TaskStatus::Working | TaskStatus::Assigned));
        if active && was != WorkerPhase::Reported {
            self.consult(
                Trigger::Exited { worker: w.clone() },
                Some(Focus::Exit {
                    worker: w.clone(),
                    code,
                }),
            );
        }
    }

    /// Approvals no rule answered are rejected; questions go to the brain.
    fn on_requests(&mut self, w: &WorkerId) {
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        if !worker.is_live() {
            return;
        }
        for p in worker.open_requests() {
            let Some(worker) = self.job.worker_mut(w) else {
                return;
            };
            worker.handled.push(p.request_id.clone());
            match p.kind {
                BlockKind::Permission => {
                    let who = match worker.config.unmatched {
                        Unmatched::Human if !self.spec.attended => Unmatched::Deny,
                        // One human question is open at a time: a second one would
                        // replace the first and leave its worker blocked for good.
                        Unmatched::Human if self.book.human.is_some() || self.book.finishing => {
                            Unmatched::Deny
                        }
                        u => u,
                    };
                    if who == Unmatched::Deny {
                        if let Some(exec) = worker.reject(&p) {
                            self.run(exec, Purpose::Input(w.clone()));
                        }
                        self.emit(Event::PermissionDenied {
                            worker: w.clone(),
                            title: p.title.clone(),
                        });
                        continue;
                    }
                    worker.phase = WorkerPhase::Blocked {
                        kind: BlockKind::Permission,
                        request_id: p.request_id.clone(),
                    };
                    self.emit(Event::WorkerBlocked {
                        worker: w.clone(),
                        kind: BlockKind::Permission,
                        title: p.title.clone(),
                    });
                    let trigger = Trigger::Approval {
                        worker: w.clone(),
                        request_id: p.request_id.clone(),
                    };
                    if who == Unmatched::Human {
                        self.ask_human(
                            trigger,
                            format!("{w} asks to: {}. Allow it?", p.title),
                            vec!["allow".to_owned(), "deny".to_owned()],
                            1,
                            None,
                        );
                    } else {
                        let detail = serde_json::to_string(&p.native).unwrap_or_default();
                        self.consult(
                            trigger,
                            Some(Focus::Approval {
                                worker: w.clone(),
                                title: p.title.clone(),
                                detail,
                            }),
                        );
                    }
                }
                BlockKind::Question => {
                    worker.phase = WorkerPhase::Blocked {
                        kind: BlockKind::Question,
                        request_id: p.request_id.clone(),
                    };
                    let questions: Vec<QuestionView> = acc_core::question::parse(&p.native)
                        .into_iter()
                        .map(|q| QuestionView {
                            question: q.question,
                            header: q.header,
                            options: q.options.into_iter().map(|o| o.label).collect(),
                            multi_select: q.multi_select,
                        })
                        .collect();
                    let questions = if questions.is_empty() {
                        vec![QuestionView {
                            question: p.title.clone(),
                            header: None,
                            options: p.options.iter().map(|o| o.label.clone()).collect(),
                            multi_select: false,
                        }]
                    } else {
                        questions
                    };
                    self.emit(Event::WorkerBlocked {
                        worker: w.clone(),
                        kind: BlockKind::Question,
                        title: p.title.clone(),
                    });
                    self.consult(
                        Trigger::Question {
                            worker: w.clone(),
                            request_id: p.request_id.clone(),
                        },
                        Some(Focus::Question {
                            worker: w.clone(),
                            questions,
                        }),
                    );
                }
                BlockKind::Auth => self.on_obs(w, Obs::Auth(p.title.clone())),
            }
        }
    }

    fn on_turn_done(&mut self, w: &WorkerId) {
        let max_turns = self.spec.budget.max_turns_per_worker;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let Some(task_id) = worker.task.clone() else {
            return;
        };
        let message = worker.last_message().unwrap_or_default();
        let reports = worker.config.access.reports();
        let result = match parse_result(&message) {
            Some(r) => r,
            None if worker.result_nudges == 0 && worker.turns < max_turns => {
                worker.result_nudges += 1;
                self.prompt_worker(w, ASK_FOR_RESULT.to_owned(), false);
                return;
            }
            None => WorkerResult {
                status: ResultStatus::Done,
                verdict: None,
                summary: tail(&strip_result(&message), 500),
                questions: Vec::new(),
                notes: "no result block; taken as done".to_owned(),
            },
        };
        let Some(task) = self.job.task_mut(&task_id) else {
            return;
        };
        if task.status != TaskStatus::Working {
            return;
        }
        let reports = reports || matches!(task.kind, TaskKind::Review { .. });
        task.status = TaskStatus::Reported;
        task.result = Some(result.clone());
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        worker.phase = WorkerPhase::Reported;
        let usage = worker.usage.clone();
        self.emit(Event::WorkerReported {
            worker: w.clone(),
            task: task_id.clone(),
            result: result.clone(),
        });
        self.emit(Event::Usage {
            worker: w.clone(),
            usage,
        });
        match result.status {
            ResultStatus::Done if reports => self.report_done(&task_id, &message, &result),
            ResultStatus::Done => self.run_diff(&task_id),
            ResultStatus::NoChange if reports => self.report_done(&task_id, &message, &result),
            ResultStatus::NoChange => {
                self.drop_task(&task_id, format!("no change needed: {}", result.summary));
            }
            ResultStatus::Blocked | ResultStatus::NeedsInput => self.consult(
                Trigger::Report {
                    task: task_id.clone(),
                    worker: w.clone(),
                },
                Some(Focus::Report {
                    task: task_id,
                    result,
                }),
            ),
        }
    }

    /// A report task finished: keep its report, hand it on, let its worker go.
    fn report_done(&mut self, task: &TaskId, message: &str, result: &WorkerResult) {
        let max = self.spec.max_report_chars;
        let mut text = strip_result(message);
        // The message is the report; the block's summary closes it.
        if text.trim().is_empty() {
            text = result.summary.clone();
        } else if !result.summary.trim().is_empty() && !text.contains(result.summary.trim()) {
            text.push_str("\n\n");
            text.push_str(result.summary.trim());
        }
        let report = cut(&text, max);
        let Some(t) = self.job.task_mut(task) else { return };
        t.status = TaskStatus::Done;
        t.report = Some(report.clone());
        let (role, kind) = (t.role.clone(), t.kind.clone());
        self.emit(Event::Artifact {
            task: task.clone(),
            role,
            chars: report.chars().count(),
        });
        self.discard_task_workers(task);
        if let TaskKind::Review { of } = kind {
            self.on_review_done(&of, task, result);
        }
    }

    /// A reviewer reported on `of`'s diff: its verdict decides, or informs
    /// the brain, as `review_by` says.
    fn on_review_done(&mut self, of: &TaskId, review_task: &TaskId, result: &WorkerResult) {
        let Some(t) = self.job.task(of) else { return };
        if t.status != TaskStatus::Reported {
            return;
        }
        let (Some(worker), Some(own_result), Some(diff)) =
            (t.worker.clone(), t.result.clone(), t.diff.clone())
        else {
            return;
        };
        let review = ReviewView {
            task: review_task.clone(),
            verdict: result.verdict,
            summary: result.summary.clone(),
        };
        let trigger = Trigger::Review {
            task: of.clone(),
            worker,
        };
        if self.spec.review_by == ReviewBy::Reviewer
            && let Some(verdict) = result.verdict
        {
            let action = match verdict {
                crate::result::Verdict::Accept => Action::Accept { task: of.clone() },
                crate::result::Verdict::Reject => Action::Reject {
                    task: of.clone(),
                    feedback: result.summary.clone(),
                },
            };
            let decision = Decision {
                action,
                rationale: result.summary.clone(),
                confidence: 1.0,
                source: DecisionSource::Reviewer {
                    task: review_task.clone(),
                },
            };
            let ask = self.next_ask();
            self.book.asks.insert(
                ask.clone(),
                PendingAsk {
                    trigger,
                    focus: Some(Focus::Review {
                        task: of.clone(),
                        result: own_result,
                        diff,
                        review: Some(review),
                    }),
                    rejections: 0,
                },
            );
            self.on_decided(ask, decision);
            return;
        }
        self.consult(
            trigger,
            Some(Focus::Review {
                task: of.clone(),
                result: own_result,
                diff,
                review: Some(review),
            }),
        );
    }

    /// A review task ended without a verdict (its worker failed, it was
    /// dropped): the brain reviews the task itself.
    fn review_task_lost(&mut self, task: &TaskId) {
        let Some(of) = self.job.task(task).and_then(|t| t.reviews().cloned()) else {
            return;
        };
        let Some(t) = self.job.task(&of) else { return };
        if t.status != TaskStatus::Reported {
            return;
        }
        let (Some(worker), Some(result), Some(diff)) = (t.worker.clone(), t.result.clone(), t.diff.clone())
        else {
            return;
        };
        self.notice(format!("{task} gave no review of {of}; the brain reviews it"));
        self.consult(
            Trigger::Review {
                task: of.clone(),
                worker,
            },
            Some(Focus::Review {
                task: of,
                result,
                diff,
                review: None,
            }),
        );
    }

    /// Sends a worker a new turn.
    fn prompt_worker(&mut self, w: &WorkerId, text: String, count: bool) {
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let exec = match worker.command(Command::prompt(text.clone())) {
            Ok(Outbound::Send(bytes)) => worker.input_exec(bytes),
            _ => None,
        };
        if count {
            worker.turns += 1;
        }
        worker.phase = WorkerPhase::Working;
        worker.last_progress_ms = now;
        let task = worker.task.clone();
        if let Some(exec) = exec {
            self.run(exec, Purpose::Input(w.clone()));
        }
        if let Some(t) = task.and_then(|t| self.job.task_mut(&t))
            && !t.status.is_terminal()
        {
            t.status = TaskStatus::Working;
            t.result = None;
            t.diff = None;
        }
        self.emit(Event::WorkerPrompted {
            worker: w.clone(),
            text,
        });
    }

    /// Replaces a worker with a new session in the same worktree: the same
    /// harness resuming its conversation, or another one primed with it.
    fn replace(&mut self, w: &WorkerId, handoff: Option<(acc_core::Harness, Option<String>)>) {
        let Some(old) = self.job.worker_mut(w) else { return };
        let req = match &handoff {
            Some((harness, prompt)) => match old.command(Command::Handoff {
                harness: *harness,
                prompt: prompt.clone(),
            }) {
                Ok(Outbound::Relaunch(req)) => Some(req),
                _ => None,
            },
            None => old.respawn_request(CONTINUE.to_owned()),
        };
        let Some(mut req) = req else {
            self.notice(format!("can't restart {w}"));
            return;
        };
        let new_id = self.new_worker_id();
        let Some(old) = self.job.worker_mut(w) else { return };
        let Some(task) = old.task.clone() else { return };
        // The predecessor is stopped here; acc's replace flag is for clients
        // that relaunch in place.
        req.replaces_parent = false;
        let mut cfg = old.config.clone();
        cfg.harness = req.harness.unwrap_or(old.harness);
        let mut next = Worker::new(
            new_id.clone(),
            task.clone(),
            cfg,
            old.host.clone(),
            old.worktree.clone(),
            self.now,
        );
        next.cwd = old.cwd.clone();
        next.respawns = old.respawns + 1;
        next.turns = old.turns;
        // Check the request now; the session itself is built at launch.
        if let Err(e) = Worker::check_launch(&req, self.now) {
            self.notice(format!("can't restart {w}: {e}"));
            self.fail_task(&task, format!("restarting failed: {e}"));
            return;
        }
        old.replaced_by = Some(new_id.clone());
        let old_alive = old.is_alive();
        let kill = old.kill_exec();
        old.phase = WorkerPhase::Gone;
        next.phase = WorkerPhase::Launching;
        next.pending_launch = Some(req);
        let harness = next.harness;
        self.job.workers.push(next);
        if let Some(t) = self.job.task_mut(&task) {
            t.attempts += 1;
            t.worker = Some(new_id.clone());
            t.status = TaskStatus::Working;
            t.result = None;
            t.diff = None;
        }
        self.out.push(Effect::Unfollow { worker: w.clone() });
        self.emit(Event::WorkerReplaced {
            from: w.clone(),
            to: new_id.clone(),
            harness,
            handoff: handoff.is_some(),
        });
        match (old_alive, kill) {
            (true, Some(exec)) => self.run(exec, Purpose::Kill(w.clone())),
            _ => self.run_pending_launch(&new_id),
        }
    }

    fn run_pending_launch(&mut self, w: &WorkerId) {
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let Some(req) = worker.pending_launch.take() else {
            return;
        };
        let rules = worker.config.policy.clone();
        let exec = match worker.launch(req, rules, now) {
            Ok(exec) => exec,
            Err(e) => {
                self.start_failed(w, format!("can't launch: {e}"));
                return;
            }
        };
        let Some(worker) = self.job.worker(w) else { return };
        let (task, harness, branch, role) = (
            worker.task.clone(),
            worker.harness,
            worker.branch.clone(),
            worker.config.role.clone(),
        );
        if let Some(task) = task {
            self.emit(Event::WorkerSpawned {
                worker: w.clone(),
                task,
                harness,
                branch,
                role,
            });
        }
        self.job.counters.spawns += 1;
        self.run(exec, Purpose::Launch(w.clone()));
    }

    fn kill(&mut self, w: &WorkerId) {
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let exec = worker.kill_exec();
        let was_live = worker.is_live();
        if !matches!(worker.phase, WorkerPhase::Discarding) {
            worker.phase = WorkerPhase::Gone;
        }
        if was_live {
            self.out.push(Effect::Unfollow { worker: w.clone() });
        }
        if let Some(exec) = exec {
            self.run(exec, Purpose::Kill(w.clone()));
        }
    }

    fn discard(&mut self, w: &WorkerId) {
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let Some(exec) = worker.discard_exec() else {
            worker.phase = WorkerPhase::Gone;
            return;
        };
        let was_live = worker.is_live();
        worker.phase = WorkerPhase::Discarding;
        if was_live {
            self.out.push(Effect::Unfollow { worker: w.clone() });
        }
        self.run(exec, Purpose::Discard(w.clone()));
    }

    fn remove(&mut self, w: &WorkerId) {
        if let Some(exec) = self.job.worker(w).and_then(Worker::remove_exec) {
            self.run(exec, Purpose::Remove(w.clone()));
        }
    }

    /// Discards every worker that worked on `task` (they share a worktree;
    /// the first discard deletes it, the rest only stop).
    fn discard_task_workers(&mut self, task: &TaskId) {
        let ids: Vec<WorkerId> = self
            .job
            .workers
            .iter()
            .filter(|w| w.task.as_ref() == Some(task) && !w.discarded)
            .map(|w| w.id.clone())
            .collect();
        let mut seen = BTreeSet::new();
        for id in ids {
            let tree = self
                .job
                .worker(&id)
                .map(|w| w.worktree.clone())
                .unwrap_or_default();
            if seen.insert(tree) {
                self.discard(&id);
            } else if let Some(w) = self.job.worker_mut(&id) {
                w.discarded = true;
                w.phase = WorkerPhase::Gone;
            }
        }
    }

    fn stop_task_workers(&mut self, task: &TaskId) {
        let ids: Vec<WorkerId> = self
            .job
            .workers
            .iter()
            .filter(|w| w.task.as_ref() == Some(task) && w.is_live())
            .map(|w| w.id.clone())
            .collect();
        for id in ids {
            self.kill(&id);
        }
    }

    // ---- tasks -------------------------------------------------------------------

    fn add_tasks(&mut self, specs: &[TaskSpec], extra_deps: &[TaskId]) -> Vec<TaskId> {
        let mut ids: Vec<TaskId> = Vec::new();
        for s in specs {
            self.book.next_task += 1;
            let id = TaskId(format!("t{}", self.book.next_task));
            let mut deps: Vec<TaskId> = s
                .depends_on
                .iter()
                .filter_map(|d| {
                    self.job
                        .tasks
                        .iter()
                        .find(|t| t.id.as_str() == d || &t.title == d)
                        .map(|t| t.id.clone())
                })
                .collect();
            deps.extend(extra_deps.iter().cloned());
            deps.dedup();
            self.push_task(
                id.clone(),
                s.title.clone(),
                s.brief.clone(),
                TaskKind::Implement,
                deps,
                s.role.clone(),
                s.harness,
                s.model.clone(),
            );
            ids.push(id);
        }
        self.book.wrap_asked = false;
        ids
    }

    #[allow(clippy::too_many_arguments)]
    fn push_task(
        &mut self,
        id: TaskId,
        title: String,
        brief: String,
        kind: TaskKind,
        depends_on: Vec<TaskId>,
        role: String,
        harness: Option<acc_core::Harness>,
        model: Option<String>,
    ) {
        self.emit(Event::TaskAdded {
            task: id.clone(),
            title: title.clone(),
            kind: kind.clone(),
            depends_on: depends_on.clone(),
            role: role.clone(),
        });
        self.job.tasks.push(Task {
            id,
            title,
            brief: if brief.trim().is_empty() {
                String::new()
            } else {
                brief
            },
            kind,
            depends_on,
            role,
            harness,
            model,
            status: TaskStatus::Pending,
            attempts: 0,
            conflicts: 0,
            worker: None,
            result: None,
            diff: None,
            report: None,
            commit: None,
            failure: None,
        });
    }

    fn drop_task(&mut self, task: &TaskId, reason: String) {
        let Some(t) = self.job.task_mut(task) else { return };
        t.status = TaskStatus::Dropped;
        self.emit(Event::TaskDropped {
            task: task.clone(),
            reason,
        });
        self.discard_task_workers(task);
        self.review_task_lost(task);
    }

    fn fail_task(&mut self, task: &TaskId, reason: String) {
        let Some(t) = self.job.task_mut(task) else { return };
        if t.status.is_terminal() {
            return;
        }
        t.status = TaskStatus::Failed;
        t.failure = Some(reason.clone());
        self.book.merge_queue.retain(|q| q != task);
        self.emit(Event::TaskFailed {
            task: task.clone(),
            reason,
        });
        // The branch stays for a human to look at.
        self.stop_task_workers(task);
        self.review_task_lost(task);
    }

    fn run_diff(&mut self, task: &TaskId) {
        let exec = self
            .job
            .task(task)
            .and_then(|t| t.worker.as_ref())
            .and_then(|w| self.job.worker(w))
            .and_then(Worker::diff_exec);
        match exec {
            Some(exec) => self.run(exec, Purpose::Diff(task.clone())),
            None => self.fail_task(task, "its worktree is gone".to_owned()),
        }
    }

    fn on_diff(&mut self, task: &TaskId, code: i32, stdout: &str, err: &str) {
        if code != 0 {
            self.fail_task(task, format!("reading its diff failed: {err}"));
            return;
        }
        let diff = cut(stdout, self.spec.max_diff_chars);
        let Some(t) = self.job.task_mut(task) else { return };
        if t.status != TaskStatus::Reported {
            return;
        }
        t.diff = Some(diff.clone());
        let title = t.title.clone();
        let (Some(worker), Some(result)) = (t.worker.clone(), t.result.clone()) else {
            return;
        };
        let trigger = Trigger::Review {
            task: task.clone(),
            worker,
        };
        if !self.spec.budget.review_required {
            let ask = self.next_ask();
            let d = Decision::rules(Action::Accept { task: task.clone() }, "review is off");
            self.book.asks.insert(
                ask.clone(),
                PendingAsk {
                    trigger,
                    focus: None,
                    rejections: 0,
                },
            );
            self.on_decided(ask, d);
            return;
        }
        if self.spec.review_by != ReviewBy::Brain && self.add_review_task(task, &title) {
            return;
        }
        self.consult(
            trigger,
            Some(Focus::Review {
                task: task.clone(),
                result,
                diff,
                review: None,
            }),
        );
    }

    /// Adds a reviewer task for `task`'s diff, when the role is on and the
    /// job has room for one more task.
    fn add_review_task(&mut self, task: &TaskId, title: &str) -> bool {
        if self.spec.resolve(REVIEWER_ROLE).is_none()
            || self.job.tasks.len() >= self.spec.budget.max_tasks as usize
        {
            return false;
        }
        self.book.next_task += 1;
        let id = TaskId(format!("t{}", self.book.next_task));
        self.push_task(
            id,
            format!("Review: {title}"),
            format!(
                "Review the diff of {task} (\"{title}\") against the goal and its brief, and give a verdict."
            ),
            TaskKind::Review { of: task.clone() },
            Vec::new(),
            REVIEWER_ROLE.to_owned(),
            None,
            None,
        );
        true
    }

    fn next_merge(&mut self) {
        if self.book.merging.is_some() || self.book.finishing {
            return;
        }
        while !self.book.merge_queue.is_empty() {
            let task = self.book.merge_queue.remove(0);
            let Some(t) = self.job.task(&task) else { continue };
            if t.status != TaskStatus::Accepted {
                continue;
            }
            let Some(w) = t.worker.clone() else { continue };
            let message = merge::message(&t.title, &self.job.id, &task, &w);
            let exec = self
                .job
                .worker(&w)
                .and_then(|x| x.merge_exec(&message, self.spec.squash));
            match exec {
                Some(exec) => {
                    self.book.merging = Some(task.clone());
                    self.run(exec, Purpose::Merge(task));
                    return;
                }
                None => self.fail_task(&task, "its worktree is gone".to_owned()),
            }
        }
    }

    fn on_merge(&mut self, task: &TaskId, code: i32, stdout: &str, err: &str) {
        self.book.merging = None;
        if code == 0 {
            let commit = stdout
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or_default()
                .to_owned();
            self.merged(task, commit);
        } else if code == acc_core::worktree::EXIT_CONFLICT as i32 {
            self.job.counters.conflicts += 1;
            if let Some(t) = self.job.task_mut(task) {
                t.status = TaskStatus::Conflicted;
                t.conflicts += 1;
            }
            self.run_conflict_files(task);
        } else if code == acc_core::worktree::EXIT_NO_WORKTREE as i32 {
            self.fail_task(task, "its worktree is gone".to_owned());
        } else {
            self.fail_task(task, format!("merging failed: {err}"));
        }
    }

    fn merged(&mut self, task: &TaskId, commit: String) {
        let Some(t) = self.job.task_mut(task) else { return };
        t.status = TaskStatus::Merged;
        t.commit = Some(commit.clone());
        let of = match &t.kind {
            TaskKind::ResolveConflict { of, .. } => Some(of.clone()),
            TaskKind::Implement | TaskKind::Review { .. } => None,
        };
        self.job.counters.merges += 1;
        self.emit(Event::Merged {
            task: task.clone(),
            commit: commit.clone(),
        });
        self.discard_task_workers(task);
        // The conflict task merged the original's branch along with its own.
        let mut of = of;
        while let Some(orig) = of {
            let Some(t) = self.job.task_mut(&orig) else { break };
            if t.status != TaskStatus::Conflicted {
                break;
            }
            t.status = TaskStatus::Merged;
            t.commit = Some(commit.clone());
            of = match &t.kind {
                TaskKind::ResolveConflict { of, .. } => Some(of.clone()),
                TaskKind::Implement | TaskKind::Review { .. } => None,
            };
            self.discard_task_workers(&orig);
        }
    }

    fn run_conflict_files(&mut self, task: &TaskId) {
        let branch = self
            .job
            .task(task)
            .and_then(|t| t.worker.as_ref())
            .and_then(|w| self.job.worker(w))
            .map(|w| w.branch.clone());
        match branch {
            Some(b) => {
                let exec = Exec::new(merge::conflict_files_script(&self.job.repo, &b));
                self.run(exec, Purpose::ConflictFiles(task.clone()));
            }
            None => self.fail_task(task, "its branch is gone".to_owned()),
        }
    }

    fn on_conflict_files(&mut self, task: &TaskId, files: Vec<String>) {
        self.emit(Event::MergeConflict {
            task: task.clone(),
            files: files.clone(),
        });
        let Some(t) = self.job.task(task) else { return };
        let budget = &self.spec.budget;
        if t.conflicts > budget.max_attempts || self.job.tasks.len() >= budget.max_tasks as usize {
            self.fail_task(task, "its merge kept conflicting".to_owned());
            return;
        }
        let Some(branch) = t
            .worker
            .as_ref()
            .and_then(|w| self.job.worker(w))
            .map(|w| w.branch.clone())
        else {
            return;
        };
        let title = format!("Resolve merge conflict: {}", t.title);
        let brief = merge::conflict_brief(&t.title, &branch, &files);
        let (harness, model, role) = (t.harness, t.model.clone(), t.role.clone());
        // Its harness can go; the branch stays until the conflict task merges it.
        self.stop_task_workers(task);
        self.book.next_task += 1;
        let id = TaskId(format!("t{}", self.book.next_task));
        let kind = TaskKind::ResolveConflict {
            of: task.clone(),
            branch,
        };
        self.push_task(id, title, brief, kind, Vec::new(), role, harness, model);
        self.book.wrap_asked = false;
    }

    // ---- the brain -----------------------------------------------------------------

    fn consult(&mut self, trigger: Trigger, focus: Option<Focus>) {
        if self.job.status.is_terminal() || self.book.finishing {
            return;
        }
        if self.book.asks.values().any(|p| p.trigger == trigger)
            || self.book.human.as_ref().is_some_and(|h| h.trigger == trigger)
        {
            return;
        }
        let ask = self.next_ask();
        let pending = PendingAsk {
            trigger,
            focus,
            rejections: 0,
        };
        if self.job.counters.consults >= self.spec.budget.max_consults {
            if !self.book.consults_exhausted {
                self.book.consults_exhausted = true;
                self.notice("the brain's consultations are used up; the rules decide from here".to_owned());
            }
            self.book.asks.insert(ask.clone(), pending);
            self.decide_by_rules(ask);
            return;
        }
        self.job.counters.consults += 1;
        self.issue_consult(ask, pending);
    }

    fn issue_consult(&mut self, ask: AskId, pending: PendingAsk) {
        self.emit(Event::Consulted {
            ask: ask.clone(),
            trigger: pending.trigger.clone(),
        });
        self.book.asks.insert(ask.clone(), pending);
        if self.spec.brain.kind == BrainKind::Rules || self.book.brain_disabled {
            let Some(pending) = self.book.asks.get(&ask) else {
                return;
            };
            let situation = self.situation(&ask, pending);
            self.out.push(Effect::Consult {
                ask,
                situation: Box::new(situation),
            });
        } else {
            self.brain_ask(ask);
        }
    }

    /// What the brain sees for `pending`.
    pub fn situation_for(&self, ask: &AskId) -> Option<Situation> {
        self.book.asks.get(ask).map(|p| self.situation(ask, p))
    }

    fn task_view(&self, t: &Task) -> TaskView {
        TaskView {
            id: t.id.clone(),
            title: t.title.clone(),
            brief: t.brief.clone(),
            kind: t.kind.clone(),
            role: t.role.clone(),
            status: t.status,
            attempts: t.attempts,
            depends_on: t.depends_on.clone(),
            worker: t.worker.clone(),
            summary: t.result.as_ref().map(|r| r.summary.clone()),
            report_excerpt: t.report.as_ref().map(|r| cut(r, EXCERPT_CHARS)),
        }
    }

    fn worker_view(&self, w: &Worker) -> WorkerView {
        WorkerView {
            id: w.id.clone(),
            task: w.task.clone(),
            role: w.config.role.clone(),
            harness: w.harness,
            phase: w.phase.name().to_owned(),
            turns: w.turns,
            respawns: w.respawns,
            stalls: w.stalls,
            plan: w.plan.clone(),
            cost_usd: w.usage.cost_usd,
            tail: tail(&w.last_message().unwrap_or_default(), TAIL_CHARS),
        }
    }

    fn role_views(&self) -> Vec<RoleView> {
        self.spec
            .enabled_roles()
            .into_iter()
            .filter(|(id, _)| id != REVIEWER_ROLE)
            .filter_map(|(id, r)| {
                let resolved = self.spec.resolve(&id)?;
                Some(RoleView {
                    id,
                    description: r.description,
                    access: r.access,
                    harness: resolved.harness,
                    model: resolved.model,
                })
            })
            .collect()
    }

    fn situation(&self, ask: &AskId, pending: &PendingAsk) -> Situation {
        let b = &self.spec.budget;
        Situation {
            protocol: crate::PROTOCOL_VERSION,
            ask: ask.clone(),
            trigger: pending.trigger.clone(),
            goal: self.job.goal.clone(),
            repo: self.job.repo.clone(),
            attended: self.spec.attended,
            harness: self.spec.workers.harness,
            alternates: self.spec.workers.alternates.clone(),
            roles: self.role_views(),
            limits: Limits {
                max_workers: b.max_workers,
                max_tasks: b.max_tasks,
                max_attempts: b.max_attempts,
                max_turns_per_worker: b.max_turns_per_worker,
            },
            tasks: self.job.tasks.iter().map(|t| self.task_view(t)).collect(),
            workers: self
                .job
                .workers
                .iter()
                .filter(|w| w.is_live() || pending.trigger.worker() == Some(&w.id))
                .map(|w| self.worker_view(w))
                .collect(),
            focus: pending.focus.clone(),
            allowed: rules::allowed(&self.job, &pending.trigger, self.cx()),
            history: self.book.history.clone(),
            notes: self.job.notes.clone(),
        }
    }

    fn remember(&mut self, ask: &AskId, trigger: &Trigger, d: &Decision, rejected: Option<String>) {
        self.book.history.push(DecisionView {
            ask: ask.clone(),
            trigger: trigger.name().to_owned(),
            action: d.action.clone(),
            source: d.source.clone(),
            rejected,
        });
        let excess = self.book.history.len().saturating_sub(HISTORY);
        self.book.history.drain(..excess);
    }

    fn on_decided(&mut self, ask: AskId, decision: Decision) {
        let Some(pending) = self.book.asks.get(&ask).cloned() else {
            return;
        };
        let allowed = rules::allowed(&self.job, &pending.trigger, self.cx());
        match rules::validate(&self.job, &allowed, &decision.action, self.cx()) {
            Ok(()) => {
                self.book.asks.remove(&ask);
                self.remember(&ask, &pending.trigger, &decision, None);
                self.emit(Event::Decided {
                    ask: ask.clone(),
                    decision: decision.clone(),
                });
                self.apply(&pending, decision.action);
            }
            Err(why) => {
                self.remember(&ask, &pending.trigger, &decision, Some(why.clone()));
                self.emit(Event::DecisionRejected {
                    ask: ask.clone(),
                    action: decision.action.clone(),
                    why,
                });
                if decision.source == DecisionSource::Rules {
                    // The rules had nothing valid to offer: stop rather than loop.
                    self.book.asks.remove(&ask);
                    self.fail_job(format!("no valid action for {}", pending.trigger.name()));
                } else if pending.rejections == 0 && decision.source != DecisionSource::Human {
                    let mut again = pending;
                    again.rejections = 1;
                    self.book.asks.remove(&ask);
                    self.issue_consult(ask, again);
                } else {
                    self.decide_by_rules(ask);
                }
            }
        }
    }

    fn decide_by_rules(&mut self, ask: AskId) {
        let Some(pending) = self.book.asks.get(&ask) else {
            return;
        };
        let situation = self.situation(&ask, pending);
        let decision = RulesBrain.decide(&situation);
        self.on_decided(ask, decision);
    }

    /// The brain couldn't answer `ask`; the rules do.
    fn brain_failed(&mut self, ask: &AskId, why: &str) {
        if self.book.asks.contains_key(ask) {
            self.notice(format!("the brain failed ({why}); the rules decide"));
            self.decide_by_rules(ask.clone());
        }
    }

    fn apply(&mut self, pending: &PendingAsk, action: Action) {
        let trigger = &pending.trigger;
        match action {
            Action::Plan { tasks } => {
                let ids = self.add_tasks(&tasks, &[]);
                self.emit(Event::Planned { tasks: ids });
                if self.job.status == JobStatus::Planning {
                    if self.spec.plan_needs_approval() {
                        self.job.status = JobStatus::AwaitingApproval;
                        let question = self.plan_question();
                        self.ask_human(
                            Trigger::Plan,
                            question,
                            vec!["approve".to_owned()],
                            1,
                            Some(PLAN_KIND),
                        );
                    } else {
                        self.job.status = JobStatus::Running;
                    }
                }
            }
            Action::Split { task, into } => {
                let deps = self
                    .job
                    .task(&task)
                    .map(|t| t.depends_on.clone())
                    .unwrap_or_default();
                if let Some(t) = self.job.task_mut(&task) {
                    t.status = TaskStatus::Dropped;
                }
                self.emit(Event::TaskDropped {
                    task: task.clone(),
                    reason: "split".to_owned(),
                });
                self.stop_task_workers(&task);
                let ids = self.add_tasks(&into, &deps);
                for t in &mut self.job.tasks {
                    if t.depends_on.contains(&task) {
                        t.depends_on.retain(|d| *d != task);
                        t.depends_on.extend(ids.iter().cloned());
                    }
                }
                self.emit(Event::Planned { tasks: ids });
            }
            Action::Prompt { worker, text } => self.prompt_worker(&worker, text, true),
            Action::Answer { worker, answers } => {
                let Some(w) = self.job.worker_mut(&worker) else {
                    return;
                };
                let WorkerPhase::Blocked { request_id, .. } = w.phase.clone() else {
                    return;
                };
                let exec = w.answer(&request_id, acc_core::question::encode(&answers));
                w.phase = WorkerPhase::Working;
                w.last_progress_ms = self.now;
                if let Some(exec) = exec {
                    self.run(exec, Purpose::Input(worker.clone()));
                }
                self.emit(Event::WorkerAnswered { worker, answers });
            }
            Action::Allow { worker } => self.settle_approval(&worker, true),
            Action::Deny { worker } => self.settle_approval(&worker, false),
            Action::Interrupt { worker } => {
                let Some(w) = self.job.worker_mut(&worker) else {
                    return;
                };
                if let Ok(Outbound::Send(bytes)) = w.command(Command::Interrupt)
                    && let Some(exec) = w.input_exec(bytes)
                {
                    self.run(exec, Purpose::Input(worker.clone()));
                }
                self.prompt_worker(&worker, WRAP_UP.to_owned(), true);
            }
            Action::Handoff {
                worker,
                harness,
                prompt,
            } => self.replace(&worker, Some((harness, prompt))),
            Action::Respawn { worker } => self.replace(&worker, None),
            Action::Accept { task } => {
                if let Some(t) = self.job.task_mut(&task) {
                    t.status = TaskStatus::Accepted;
                }
                self.emit(Event::Reviewed {
                    task: task.clone(),
                    accepted: true,
                    feedback: None,
                });
                self.book.merge_queue.push(task);
            }
            Action::Reject { task, feedback } => {
                self.emit(Event::Reviewed {
                    task: task.clone(),
                    accepted: false,
                    feedback: Some(feedback.clone()),
                });
                let Some(t) = self.job.task(&task) else { return };
                let Some(w) = t.worker.clone() else { return };
                let Some(worker) = self.job.worker(&w) else { return };
                let (branch, cfg) = (worker.branch.clone(), worker.config.clone());
                let text = self.worker_prompt(t, &cfg, &branch, Some(&feedback));
                self.prompt_worker(&w, text, true);
            }
            Action::Wait { ms } => {
                if let Some(w) = trigger.worker().and_then(|w| self.job.worker_mut(w))
                    && matches!(w.phase, WorkerPhase::Stalled { .. })
                {
                    w.phase = WorkerPhase::Working;
                    w.last_progress_ms = self.now.saturating_add(ms);
                }
                self.out.push(Effect::WakeIn { ms });
            }
            Action::AskHuman { question, options } => {
                let questions = match &pending.focus {
                    Some(Focus::Question { questions, .. }) => questions.len(),
                    _ => 1,
                };
                self.ask_human(trigger.clone(), question, options, questions, None);
            }
            Action::GiveUpTask { task, reason } => self.fail_task(&task, reason),
            Action::GiveUp { reason } => self.fail_job(reason),
            Action::Done { summary } => self.finish(JobStatus::Done, summary),
        }
    }

    /// The plan, for the human to approve.
    fn plan_question(&self) -> String {
        let mut q = String::from("The plan:\n");
        for t in self.job.tasks.iter().filter(|t| t.status == TaskStatus::Pending) {
            let deps = if t.depends_on.is_empty() {
                String::new()
            } else {
                let d: Vec<&str> = t.depends_on.iter().map(TaskId::as_str).collect();
                format!(" (after {})", d.join(", "))
            };
            q.push_str(&format!("- {} [{}] {}{deps}\n", t.id, t.role, t.title));
        }
        q.push_str("Reply `approve` to start, or say what to change.");
        q
    }

    // ---- the orchestrator's session -----------------------------------------------

    /// Puts `ask` to the brain's session: a turn now, a fresh session when
    /// none runs, or a place in the queue while it thinks.
    fn brain_ask(&mut self, ask: AskId) {
        let Some(pending) = self.book.asks.get(&ask).cloned() else {
            return;
        };
        let situation = self.situation(&ask, &pending);
        match self.book.brain.as_ref().map(|b| (b.phase.clone(), b.turns)) {
            Some((BrainPhase::Thinking { .. } | BrainPhase::Launching, _)) => {
                if !self.book.brain_queue.contains(&ask) {
                    self.book.brain_queue.push(ask);
                }
            }
            Some((BrainPhase::Ready, turns))
                if self.spec.brain.kind == BrainKind::Session && turns < self.spec.brain.max_turns =>
            {
                let delta = self.brain_delta(&situation);
                let text = wire::render_turn(&situation, &delta, &self.spec.prompts);
                let now = self.now;
                let Some(brain) = self.book.brain.as_mut() else {
                    return;
                };
                brain.phase = BrainPhase::Thinking { ask, since_ms: now };
                brain.turns += 1;
                brain.retried = false;
                let id = brain.id.clone();
                if let Some(exec) = brain.prompt_exec(text) {
                    self.run(exec, Purpose::BrainInput(id));
                }
                self.brain_mark_seen();
            }
            Some((BrainPhase::Ready, turns)) => {
                let why = if self.spec.brain.kind == BrainKind::Session {
                    format!("retired after {turns} turns; a fresh session takes over")
                } else {
                    "one consultation, one session".to_owned()
                };
                self.brain_retire(&why);
                self.brain_launch(ask, &situation);
            }
            Some((BrainPhase::Gone, _)) | None => self.brain_launch(ask, &situation),
        }
    }

    fn brain_launch(&mut self, ask: AskId, situation: &Situation) {
        self.book.next_brain += 1;
        let id = brain_id(&self.job.id, self.book.next_brain);
        let prompt = wire::render_prompt(situation, &self.spec.prompts);
        let req = BrainSession::request(
            &self.job.id,
            &self.spec.brain,
            self.spec.workers.harness,
            &self.job.repo,
            prompt.clone(),
        );
        match BrainSession::launch(id.clone(), &self.spec.brain, req, prompt, ask.clone(), self.now) {
            Ok((brain, exec)) => {
                self.emit(Event::BrainStarted {
                    session: id.clone(),
                    harness: brain.harness,
                    model: brain.model.clone(),
                });
                self.book.brain = Some(brain);
                self.brain_mark_seen();
                self.run(exec, Purpose::BrainLaunch(id));
            }
            Err(e) => {
                self.book.brain = None;
                self.brain_launch_failed(&ask, &format!("can't launch the orchestrator's harness: {e}"));
            }
        }
    }

    fn brain_launch_failed(&mut self, ask: &AskId, why: &str) {
        self.book.brain_launch_failures += 1;
        if self.book.brain_launch_failures >= BRAIN_LAUNCH_FAILURES && !self.book.brain_disabled {
            self.book.brain_disabled = true;
            self.notice(format!(
                "the orchestrator's harness failed to start {} times; the rules decide from here",
                self.book.brain_launch_failures
            ));
        }
        self.brain_failed(ask, why);
        self.brain_pump();
    }

    fn on_brain_launched(&mut self, id: &str, code: i32, stdout: &str, err: &str) {
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id) else {
            return;
        };
        if code != 0 {
            let ask = brain.thinking_about().cloned();
            brain.phase = BrainPhase::Gone;
            let remove = brain.remove_exec();
            self.emit(Event::BrainEnded {
                session: id.to_owned(),
                why: format!("starting the harness failed: {err}"),
            });
            if let Some(exec) = remove {
                self.run(exec, Purpose::BrainRemove(id.to_owned()));
            }
            if let Some(ask) = ask {
                self.brain_launch_failed(&ask, &format!("starting the harness failed: {err}"));
            }
            return;
        }
        self.book.brain_launch_failures = 0;
        let updates = brain.on_launched(stdout);
        self.out.push(Effect::Follow {
            worker: WorkerId(id.to_owned()),
            host: self.job.host.clone(),
            offset: 0,
        });
        self.on_brain_updates(id, &updates);
    }

    fn on_brain_bytes(&mut self, id: &str, bytes: &[u8]) {
        let now = self.now;
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id) else {
            return;
        };
        let updates = brain.feed(bytes, now);
        self.on_brain_updates(id, &updates);
    }

    fn on_brain_log_ended(&mut self, id: &str, code: Option<i32>) {
        let now = self.now;
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id) else {
            return;
        };
        let updates = brain.on_exit(code, now);
        self.on_brain_updates(id, &updates);
        if self
            .book
            .brain
            .as_ref()
            .is_some_and(|b| b.id == id && b.is_live())
        {
            self.on_brain_exited(id, code);
        }
    }

    fn on_brain_updates(&mut self, id: &str, updates: &[Update]) {
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id) else {
            return;
        };
        let was_replaying = updates.iter().any(|u| u.replay);
        let obs = brain.observe(updates);
        let caught_up = was_replaying && !brain.is_replaying();
        self.job.brain_cost_usd = self.book.brain_cost_retired + brain.cost_usd;
        for o in obs {
            self.on_brain_obs(id, o);
        }
        if caught_up {
            self.brain_reconcile(id);
        }
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id && b.is_live()) else {
            return;
        };
        match brain.answer_requests() {
            Ok(execs) => {
                for exec in execs {
                    self.run(exec, Purpose::BrainInput(id.to_owned()));
                }
            }
            Err(why) => {
                self.on_brain_obs(id, BrainObs::Auth(why));
                return;
            }
        }
        let now = self.now;
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id && b.is_live()) else {
            return;
        };
        if let Some(cmds) = brain.deferred_due(now) {
            brain.deferred = None;
            if let BrainPhase::Thinking { since_ms, .. } = &mut brain.phase {
                *since_ms = now;
            }
            let mut execs = Vec::new();
            for cmd in cmds {
                execs.extend(brain.command_exec(cmd));
            }
            for exec in execs {
                self.run(exec, Purpose::BrainInput(id.to_owned()));
            }
        }
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id) else {
            return;
        };
        let (input, spec) = brain.flush();
        if let Some(exec) = input {
            self.run(exec, Purpose::BrainInput(id.to_owned()));
        }
        if let Some(exec) = spec {
            self.run(exec, Purpose::BrainSpecWrite(id.to_owned()));
        }
    }

    /// After replaying the brain's log on restore: a turn that ended while
    /// jefe was away is read now.
    fn brain_reconcile(&mut self, id: &str) {
        let Some(brain) = self.book.brain.as_ref().filter(|b| b.id == id && b.is_live()) else {
            return;
        };
        if !brain.is_alive() {
            self.on_brain_exited(id, None);
            return;
        }
        if brain.thinking_about().is_some()
            && let Some(acc_core::RunState::Done | acc_core::RunState::Idle | acc_core::RunState::Error) =
                brain.state()
        {
            let end = brain.turn_end();
            self.on_brain_obs(id, end);
        }
    }

    fn on_brain_obs(&mut self, id: &str, o: BrainObs) {
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id && b.is_live()) else {
            return;
        };
        match o {
            BrainObs::TurnDone => {
                // A turn before the first prompt (held for effort) is nothing.
                if brain.deferred.is_some() {
                    return;
                }
                let Some(ask) = brain.thinking_about().cloned() else {
                    brain.phase = BrainPhase::Ready;
                    return;
                };
                let reply = brain.last_message().unwrap_or_default();
                let source = DecisionSource::Brain {
                    harness: Some(brain.harness),
                    model: brain.model.clone(),
                };
                let decision = wire::parse_decision(&reply, source);
                let text = cut(&wire::strip_decision(&reply), BRAIN_TEXT_CHARS);
                self.emit(Event::BrainTurn {
                    session: id.to_owned(),
                    ask: Some(ask.clone()),
                    text,
                });
                let Some(brain) = self.book.brain.as_mut() else {
                    return;
                };
                match decision {
                    Some(d) => {
                        brain.phase = BrainPhase::Ready;
                        if self.spec.brain.kind == BrainKind::Oneshot {
                            self.brain_retire("one consultation, one session");
                        }
                        self.on_decided(ask, d);
                        self.brain_pump();
                    }
                    None if !brain.retried => {
                        brain.retried = true;
                        brain.turns += 1;
                        if let BrainPhase::Thinking { since_ms, .. } = &mut brain.phase {
                            *since_ms = self.now;
                        }
                        if let Some(exec) = brain.prompt_exec(ASK_FOR_BLOCK.to_owned()) {
                            self.run(exec, Purpose::BrainInput(id.to_owned()));
                        }
                    }
                    None => {
                        brain.phase = BrainPhase::Ready;
                        if self.spec.brain.kind == BrainKind::Oneshot {
                            self.brain_retire("one consultation, one session");
                        }
                        self.brain_failed(&ask, "no valid jefe-decision block, twice");
                        self.brain_pump();
                    }
                }
            }
            BrainObs::TurnError(message) => {
                let ask = brain.thinking_about().cloned();
                brain.phase = BrainPhase::Ready;
                if let Some(ask) = ask {
                    self.brain_failed(&ask, &message);
                }
                self.brain_pump();
            }
            BrainObs::Auth(message) => {
                let ask = brain.thinking_about().cloned();
                self.brain_retire(&format!("its harness needs a login: {message}"));
                if let Some(ask) = ask {
                    self.brain_failed(
                        &ask,
                        &format!("the orchestrator's harness needs a login: {message}"),
                    );
                }
                self.book.brain_disabled = true;
                self.notice(
                    "the orchestrator's harness needs a login; the rules decide from here".to_owned(),
                );
                self.brain_pump();
            }
            BrainObs::Exited(code) => self.on_brain_exited(id, code),
        }
    }

    fn on_brain_exited(&mut self, id: &str, code: Option<i32>) {
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.id == id && b.is_live()) else {
            return;
        };
        let ask = brain.thinking_about().cloned();
        brain.phase = BrainPhase::Gone;
        self.book.brain_cost_retired += brain.cost_usd;
        let remove = brain.remove_exec();
        self.out.push(Effect::Unfollow {
            worker: WorkerId(id.to_owned()),
        });
        self.emit(Event::BrainEnded {
            session: id.to_owned(),
            why: format!(
                "its harness exited ({})",
                code.map_or("no code".to_owned(), |c| c.to_string())
            ),
        });
        if let Some(exec) = remove {
            self.run(exec, Purpose::BrainRemove(id.to_owned()));
        }
        if let Some(ask) = ask {
            self.brain_failed(&ask, "the orchestrator's harness exited before deciding");
        }
        self.brain_pump();
    }

    /// Stops the current brain session (its asks are the caller's business).
    fn brain_retire(&mut self, why: &str) {
        let Some(brain) = self.book.brain.as_mut().filter(|b| b.is_live()) else {
            return;
        };
        brain.phase = BrainPhase::Gone;
        self.book.brain_cost_retired += brain.cost_usd;
        let id = brain.id.clone();
        let kill = brain.is_alive().then(|| brain.kill_exec()).flatten();
        let remove = brain.remove_exec();
        self.out.push(Effect::Unfollow {
            worker: WorkerId(id.clone()),
        });
        self.emit(Event::BrainEnded {
            session: id.clone(),
            why: why.to_owned(),
        });
        match (kill, remove) {
            (Some(kill), remove) => {
                // The files go once the harness has stopped; a later session
                // may have taken `book.brain` by then, so the exec is kept here.
                if let Some(remove) = remove {
                    self.book.brain_removals.insert(id.clone(), remove);
                }
                self.run(kill, Purpose::BrainKill(id));
            }
            (None, Some(remove)) => self.run(remove, Purpose::BrainRemove(id)),
            (None, None) => {}
        }
    }

    /// Deletes a stopped brain session's files, once its kill is done.
    fn brain_remove(&mut self, id: &str) {
        if let Some(exec) = self.book.brain_removals.remove(id) {
            self.run(exec, Purpose::BrainRemove(id.to_owned()));
        }
    }

    /// Puts the next waiting ask to the brain, if it is free.
    fn brain_pump(&mut self) {
        if self.book.brain_disabled {
            // Whatever waited for the brain goes to the rules.
            let queue = std::mem::take(&mut self.book.brain_queue);
            for ask in queue {
                self.decide_by_rules(ask);
            }
            return;
        }
        if matches!(
            self.book.brain.as_ref().map(|b| &b.phase),
            Some(BrainPhase::Thinking { .. } | BrainPhase::Launching)
        ) {
            return;
        }
        while !self.book.brain_queue.is_empty() {
            let ask = self.book.brain_queue.remove(0);
            if self.book.asks.contains_key(&ask) {
                self.brain_ask(ask);
                return;
            }
        }
    }

    /// What changed since the brain's last turn.
    fn brain_delta(&self, s: &Situation) -> serde_json::Value {
        let seen = &self.book.brain_seen;
        let tasks: Vec<&TaskView> = s
            .tasks
            .iter()
            .filter(|t| seen.tasks.get(&t.id) != Some(&(t.status, t.attempts, t.worker.clone())))
            .collect();
        let workers: Vec<&WorkerView> = s
            .workers
            .iter()
            .filter(|w| seen.workers.get(&w.id) != Some(&w.phase))
            .collect();
        let notes: Vec<&String> = s.notes.iter().skip(seen.notes).collect();
        let rejected = self
            .book
            .history
            .last()
            .filter(|h| h.ask == s.ask)
            .and_then(|h| h.rejected.clone());
        serde_json::json!({
            "ask": s.ask,
            "trigger": s.trigger,
            "focus": s.focus,
            "allowed": s.allowed,
            "tasks": tasks,
            "workers": workers,
            "notes": notes,
            "rejected": rejected,
            "limits": s.limits,
        })
    }

    fn brain_mark_seen(&mut self) {
        let seen = &mut self.book.brain_seen;
        seen.tasks = self
            .job
            .tasks
            .iter()
            .map(|t| (t.id.clone(), (t.status, t.attempts, t.worker.clone())))
            .collect();
        seen.workers = self
            .job
            .workers
            .iter()
            .map(|w| (w.id.clone(), w.phase.name().to_owned()))
            .collect();
        seen.notes = self.job.notes.len();
    }

    // ---- the human ---------------------------------------------------------------------

    fn ask_human(
        &mut self,
        trigger: Trigger,
        question: String,
        options: Vec<String>,
        questions: usize,
        kind: Option<&str>,
    ) {
        let ask = self.next_ask();
        self.book.human = Some(HumanAsk {
            ask: ask.clone(),
            trigger,
            questions,
            kind: kind.map(str::to_owned),
        });
        if kind != Some(PLAN_KIND) {
            self.job.status = JobStatus::NeedsHuman;
        }
        self.emit(Event::HumanAsked {
            ask: ask.clone(),
            question: question.clone(),
            options: options.clone(),
            kind: kind.map(str::to_owned),
        });
        self.out.push(Effect::AskHuman {
            ask,
            question,
            options,
            kind: kind.map(str::to_owned),
        });
    }

    /// Answers the approval `w` waits on.
    fn settle_approval(&mut self, w: &WorkerId, allow: bool) {
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let WorkerPhase::Blocked { request_id, .. } = worker.phase.clone() else {
            return;
        };
        let Some(p) = worker.session().and_then(|s| {
            s.permissions()
                .iter()
                .find(|p| p.request_id == request_id)
                .cloned()
        }) else {
            worker.phase = WorkerPhase::Working;
            return;
        };
        let exec = if allow {
            acc_core::policy::option_for(acc_core::policy::Decision::Allow, &p.options)
                .map(|o| o.id.clone())
                .and_then(|id| worker.answer(&request_id, id))
        } else {
            worker.reject(&p)
        };
        worker.phase = WorkerPhase::Working;
        worker.last_progress_ms = now;
        if let Some(exec) = exec {
            self.run(exec, Purpose::Input(w.clone()));
        }
        let title = p.title.clone();
        self.emit(if allow {
            Event::PermissionGranted {
                worker: w.clone(),
                title,
            }
        } else {
            Event::PermissionDenied {
                worker: w.clone(),
                title,
            }
        });
    }

    fn on_human(&mut self, reply: HumanReply) {
        match reply {
            HumanReply::Cancel => self.finish(JobStatus::Cancelled, "cancelled".to_owned()),
            HumanReply::Nudge { text } => {
                self.notice(format!("note from the human: {text}"));
                self.job.notes.push(text);
            }
            HumanReply::Message { text, consult } => {
                if !text.trim().is_empty() {
                    self.notice(format!("note from the human: {text}"));
                    self.job.notes.push(text);
                }
                if consult && !self.job.status.is_terminal() {
                    self.consult(Trigger::Nudge, None);
                }
            }
            HumanReply::Answer { ask, text } => {
                let Some(h) = self.book.human.clone() else { return };
                if ask.as_ref().is_some_and(|a| *a != h.ask) {
                    return;
                }
                self.book.human = None;
                self.emit(Event::HumanAnswered {
                    ask: h.ask.clone(),
                    text: text.clone(),
                });
                if h.kind.as_deref() == Some(PLAN_KIND) {
                    self.on_plan_answer(&text);
                    return;
                }
                self.job.status = JobStatus::Running;
                let action = self.human_action(&h, &text);
                let Some(action) = action else {
                    // Plan and wrap-up: the answer is a note for the brain, asked again.
                    self.job.notes.push(text);
                    self.consult(h.trigger, None);
                    return;
                };
                let decision = Decision {
                    action,
                    rationale: "the human said so".to_owned(),
                    confidence: 1.0,
                    source: DecisionSource::Human,
                };
                self.book.asks.insert(
                    h.ask.clone(),
                    PendingAsk {
                        trigger: h.trigger,
                        focus: None,
                        rejections: 0,
                    },
                );
                self.on_decided(h.ask, decision);
            }
        }
    }

    /// The human's answer to the plan: start, or plan again with the
    /// feedback as a note.
    fn on_plan_answer(&mut self, text: &str) {
        let yes = matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "y" | "yes" | "ok" | "approve" | "approved" | "go" | "lgtm" | "start"
        );
        if yes {
            self.job.status = JobStatus::Running;
            self.emit(Event::PlanApproved);
            return;
        }
        // The planned tasks never started: take them out of the way.
        let pending: Vec<TaskId> = self
            .job
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .map(|t| t.id.clone())
            .collect();
        for id in &pending {
            self.emit(Event::TaskDropped {
                task: id.clone(),
                reason: "the plan is being revised".to_owned(),
            });
        }
        self.job.tasks.retain(|t| !pending.contains(&t.id));
        self.notice(format!("note from the human: {text}"));
        self.job.notes.push(text.to_owned());
        self.job.status = JobStatus::Planning;
        self.consult(Trigger::Plan, None);
    }

    /// What a human's free-text answer means for the question it answers.
    fn human_action(&self, h: &HumanAsk, text: &str) -> Option<Action> {
        let trigger = &h.trigger;
        let yes = matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "y" | "yes" | "ok" | "accept" | "lgtm" | "merge"
        );
        match trigger {
            Trigger::Plan | Trigger::Wrap | Trigger::Nudge => None,
            Trigger::Review { task, .. } if yes => Some(Action::Accept { task: task.clone() }),
            Trigger::Review { task, .. } => Some(Action::Reject {
                task: task.clone(),
                feedback: text.to_owned(),
            }),
            Trigger::Question { worker, .. } => Some(Action::Answer {
                worker: worker.clone(),
                answers: vec![vec![text.to_owned()]; h.questions.max(1)],
            }),
            Trigger::Auth { worker } => Some(Action::Respawn {
                worker: worker.clone(),
            }),
            Trigger::Approval { worker, .. } if yes || text.trim().eq_ignore_ascii_case("allow") => {
                Some(Action::Allow {
                    worker: worker.clone(),
                })
            }
            Trigger::Approval { worker, .. } => Some(Action::Deny {
                worker: worker.clone(),
            }),
            Trigger::Report { worker, .. }
            | Trigger::WorkerError { worker }
            | Trigger::Stalled { worker }
            | Trigger::Quota { worker } => Some(Action::Prompt {
                worker: worker.clone(),
                text: text.to_owned(),
            }),
            Trigger::Exited { worker } => Some(Action::Respawn {
                worker: worker.clone(),
            }),
        }
    }

    // ---- time ------------------------------------------------------------------------------

    fn on_tick(&mut self) {
        if self.job.status.is_terminal() {
            return;
        }
        let b = self.spec.budget.clone();
        if self.now.saturating_sub(self.job.created_ms) > b.wall_ms {
            self.fail_job(format!(
                "the job ran past its {} minute limit",
                b.wall_ms / 60_000
            ));
            return;
        }
        let due: Vec<WorkerId> = self
            .job
            .workers
            .iter()
            .filter(|w| w.resend_at.is_some_and(|at| at <= self.now))
            .map(|w| w.id.clone())
            .collect();
        for w in due {
            let Some(worker) = self.job.worker_mut(&w) else {
                continue;
            };
            worker.resend_at = None;
            let execs = std::mem::take(&mut worker.unsent);
            if !worker.is_live() || !worker.has_session() {
                continue;
            }
            self.notice(format!(
                "{w} has been silent since the restart; writing again what may not have reached it"
            ));
            for exec in execs {
                self.run(exec, Purpose::Input(w.clone()));
            }
        }
        let deferred: Vec<WorkerId> = self
            .job
            .workers
            .iter()
            .filter(|w| w.deferred.is_some() && w.is_live())
            .map(|w| w.id.clone())
            .collect();
        for w in deferred {
            self.send_deferred(&w);
            self.flush(&w);
        }
        let stalled: Vec<(WorkerId, u64)> = self
            .job
            .workers
            .iter()
            .filter(|w| {
                w.is_live()
                    && w.deferred.is_none()
                    && matches!(w.phase, WorkerPhase::Working | WorkerPhase::Waiting)
            })
            .map(|w| (w.id.clone(), self.now.saturating_sub(w.last_progress_ms)))
            .filter(|(_, idle)| *idle > b.stall_ms)
            .collect();
        for (w, idle_ms) in stalled {
            if let Some(worker) = self.job.worker_mut(&w) {
                worker.phase = WorkerPhase::Stalled { since_ms: self.now };
                worker.stalls += 1;
            }
            self.emit(Event::WorkerStalled {
                worker: w.clone(),
                idle_ms,
            });
            self.consult(
                Trigger::Stalled { worker: w.clone() },
                Some(Focus::Stall { worker: w, idle_ms }),
            );
        }
        self.brain_tick(b.brain_timeout_ms);
    }

    /// The brain's held-back first prompt, and its turn timeout.
    fn brain_tick(&mut self, timeout_ms: u64) {
        let Some(brain) = self.book.brain.as_ref().filter(|b| b.is_live()) else {
            return;
        };
        let id = brain.id.clone();
        if brain.deferred.is_some() {
            self.on_brain_updates(&id, &[]);
            return;
        }
        if let BrainPhase::Thinking { ask, since_ms } = &brain.phase
            && self.now.saturating_sub(*since_ms) > timeout_ms
        {
            let ask = ask.clone();
            self.brain_retire(&format!("no decision within {} s", timeout_ms / 1000));
            self.brain_failed(&ask, &format!("no decision within {} s", timeout_ms / 1000));
            self.brain_pump();
        }
    }

    // ---- the end ---------------------------------------------------------------------------

    fn maybe_wrap(&mut self) {
        if self.book.wrap_asked
            || self.job.tasks.is_empty()
            || self.book.merging.is_some()
            || !self.book.merge_queue.is_empty()
            || !self.book.asks.is_empty()
            || self.book.human.is_some()
            || !self.job.tasks.iter().all(|t| t.status.is_terminal())
        {
            return;
        }
        self.book.wrap_asked = true;
        self.consult(Trigger::Wrap, None);
    }

    /// Branches with work left on them.
    fn kept_branches(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in &self.job.workers {
            if w.cwd.is_some() && !w.discarded && !out.contains(&w.branch) {
                let landed = w
                    .task
                    .as_ref()
                    .and_then(|t| self.job.task(t))
                    .is_some_and(|t| t.status.landed());
                if !landed && w.config.access == Access::Worktree {
                    out.push(w.branch.clone());
                }
            }
        }
        out
    }

    fn report(&self, summary: String) -> Report {
        Report {
            status: self.job.status,
            summary,
            merged: self
                .job
                .tasks
                .iter()
                // A conflict task's merge is listed under the task it landed.
                .filter(|t| {
                    t.status == TaskStatus::Merged && !matches!(t.kind, TaskKind::ResolveConflict { .. })
                })
                .map(|t| MergedTask {
                    task: t.id.clone(),
                    title: t.title.clone(),
                    commit: t.commit.clone().unwrap_or_default(),
                })
                .collect(),
            branches: self.kept_branches(),
        }
    }

    fn fail_job(&mut self, reason: String) {
        self.finish(JobStatus::Failed, reason);
    }

    fn finish(&mut self, status: JobStatus, summary: String) {
        if self.job.status.is_terminal() {
            return;
        }
        let live: Vec<WorkerId> = self
            .job
            .workers
            .iter()
            .filter(|w| w.is_live())
            .map(|w| w.id.clone())
            .collect();
        for w in live {
            self.kill(&w);
        }
        self.brain_retire("the job is over");
        self.book.asks.clear();
        self.book.brain_queue.clear();
        self.book.human = None;
        self.book.merge_queue.clear();
        self.job.status = status;
        let report = self.report(summary.clone());
        let branches = report.branches.clone();
        self.emit(match status {
            JobStatus::Done => Event::JobDone {
                summary,
                merged: report.merged.clone(),
                branches,
            },
            JobStatus::Cancelled => Event::JobCancelled { branches },
            _ => Event::JobFailed {
                reason: summary,
                branches,
            },
        });
        self.job.report = Some(report);
        self.book.finishing = true;
    }
}

/// The last `max` characters of `text`.
fn tail(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_owned();
    }
    let skip = n - max;
    format!("…{}", text.chars().skip(skip).collect::<String>())
}

/// `text` cut to `max` bytes at a character boundary, saying so.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let end = (0..=max).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
    format!("{}\n[cut at {max} characters]", &text[..end])
}
