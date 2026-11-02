//! The supervision loop: a state machine fed [`Input`]s that returns
//! [`Effect`]s. Deterministic for a given input sequence and clock, so a job
//! can be replayed from its input log.

use std::collections::{BTreeMap, BTreeSet};

use acc_core::command::Command;
use acc_core::session::{LaunchRequest, Outbound, SessionSpec, Update};
use acc_core::{BlockKind, Exec};
use serde::{Deserialize, Serialize};

use crate::brain::{
    Action, Brain, Decision, DecisionSource, DecisionView, Focus, Limits, QuestionView, RulesBrain,
    Situation, TaskView, Trigger, WorkerView,
};
use crate::event::{Event, Stamped};
use crate::ids::{AskId, ExecId, TaskId, WorkerId, worker_id};
use crate::io::{Effect, HumanReply, Input, Purpose};
use crate::job::{
    Counters, Job, JobSpec, JobStatus, MergedTask, Report, Task, TaskKind, TaskSpec, TaskStatus,
};
use crate::merge;
use crate::prompts::render;
use crate::result::{ResultStatus, WorkerResult, parse_result, strip_result};
use crate::rules::{self, Context};
use crate::worker::{Obs, Worker, WorkerPhase};

/// Decisions kept in a situation's history.
const HISTORY: usize = 12;
/// Characters of a worker's last message shown to the brain.
const TAIL_CHARS: usize = 2_000;

const ASK_FOR_RESULT: &str = "End your final message with the ```jefe-result block described in your instructions \
     (status done, no_change, needs_input or blocked, and a summary).";
const WRAP_UP: &str = "You were interrupted because you stopped making progress. Wrap up: commit what works and \
     report with a ```jefe-result block.";
const CONTINUE: &str = "Your session was restarted. Continue the task where you left off, and end with a \
     ```jefe-result block.";

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
    /// The job is over; waiting for cleanup execs before `Finished`.
    finishing: bool,
    finished: bool,
}

#[derive(Serialize, Deserialize)]
struct SnapshotDoc {
    v: u32,
    spec: JobSpec,
    job: serde_json::Value,
    book: Book,
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

    /// When the loop next needs a [`Input::Tick`]: the earliest stall or
    /// wall-clock deadline.
    pub fn next_deadline(&self) -> Option<u64> {
        if self.job.status.is_terminal() {
            return None;
        }
        let b = &self.spec.budget;
        let wall = self.job.created_ms.saturating_add(b.wall_ms);
        self.job
            .workers
            .iter()
            .filter(|w| matches!(w.phase, WorkerPhase::Working | WorkerPhase::Waiting))
            .map(|w| w.last_progress_ms.saturating_add(b.stall_ms))
            .chain(self.job.workers.iter().filter_map(|w| w.resend_at))
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
                let updates = match self.job.worker_mut(&worker) {
                    Some(w) => w.feed(&bytes, now_ms),
                    None => Vec::new(),
                };
                self.on_updates(&worker, &updates);
            }
            Input::LogEnded { worker, code } => {
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
            Input::Decided { ask, decision } => self.on_decided(ask, decision),
            Input::BrainFailed { ask, why } => {
                if self.book.asks.contains_key(&ask) {
                    self.notice(format!("the brain failed ({why}); the rules decide"));
                    self.decide_by_rules(ask);
                }
            }
            Input::Human(reply) => self.on_human(reply),
        }
        self.settle();
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
        let doc = serde_json::json!({
            "v": SNAPSHOT_VERSION,
            "spec": &self.spec,
            "job": job,
            "book": &self.book,
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
        let lost: Vec<(ExecId, Purpose)> = doc.book.runs.clone().into_iter().collect();
        let mut j = Self {
            spec: doc.spec,
            job,
            book: doc.book,
            now: now_ms,
            out: Vec::new(),
        };
        j.book.runs.clear();
        let mut lost_inputs = std::mem::take(&mut j.book.inputs);
        if j.book.finished {
            return Ok((j, Vec::new()));
        }
        let rules = crate::policy::worker_rules(&j.spec.workers.policy);
        let mut gone = Vec::new();
        for w in &mut j.job.workers {
            if w.discarded || (matches!(w.phase, WorkerPhase::Gone) && w.cwd.is_none()) {
                continue;
            }
            match specs.iter().find(|s| s.id == w.id.as_str()) {
                Some(spec) => {
                    if w.reattach(spec.clone(), rules.clone()).is_err() {
                        gone.push(w.id.clone());
                    }
                }
                None if w.phase == WorkerPhase::Preparing || w.pending_launch.is_some() => {}
                None => gone.push(w.id.clone()),
            }
        }
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
                Purpose::Launch(_) | Purpose::Status(_) | Purpose::SpecWrite(_) => {}
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
        for id in gone {
            if let Some(w) = j.job.worker_mut(&id)
                && w.is_live()
            {
                j.on_exited(&id, None);
            }
        }
        let asks: Vec<(AskId, PendingAsk)> = std::mem::take(&mut j.book.asks).into_iter().collect();
        for (ask, pending) in asks {
            j.issue_consult(ask, pending);
        }
        if let Some(h) = j.book.human.clone() {
            j.out.push(Effect::AskHuman {
                ask: h.ask,
                question: "(asked before the restart; see the log)".to_owned(),
                options: Vec::new(),
            });
        }
        j.notice("resumed".to_owned());
        j.settle();
        let out = std::mem::take(&mut j.out);
        Ok((j, out))
    }

    // ---- plumbing ------------------------------------------------------------

    fn cx(&self) -> Context<'_> {
        Context {
            budget: &self.spec.budget,
            attended: self.spec.attended,
            alternates: &self.spec.workers.alternates,
        }
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
            let deps: Vec<TaskStatus> = self
                .job
                .task(&id)
                .map(|t| {
                    t.depends_on
                        .iter()
                        .filter_map(|d| self.job.task(d))
                        .map(|d| d.status)
                        .collect()
                })
                .unwrap_or_default();
            if deps
                .iter()
                .any(|s| matches!(s, TaskStatus::Failed | TaskStatus::Dropped))
            {
                self.drop_task(&id, "a task it depends on did not land".to_owned());
                continue;
            }
            if !deps.iter().all(|s| *s == TaskStatus::Merged) {
                continue;
            }
            if self.job.live_workers() >= self.spec.budget.max_workers as usize {
                break;
            }
            self.spawn(&id);
        }
    }

    fn new_worker_id(&mut self) -> WorkerId {
        self.book.next_worker += 1;
        worker_id(&self.job.id, self.book.next_worker)
    }

    fn spawn(&mut self, task: &TaskId) {
        let id = self.new_worker_id();
        let Some(t) = self.job.task_mut(task) else { return };
        let harness = t.harness.unwrap_or(self.spec.workers.harness);
        t.status = TaskStatus::Assigned;
        t.worker = Some(id.clone());
        t.attempts += 1;
        let worker = Worker::new(
            id.clone(),
            task.clone(),
            harness,
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

    fn worker_prompt(&self, task: &Task, branch: &str, feedback: Option<&str>) -> String {
        let feedback = feedback
            .map(|f| format!("\nFeedback on your previous attempt:\n\n{f}\n"))
            .unwrap_or_default();
        let vars = BTreeMap::from([
            ("goal", self.job.goal.clone()),
            ("task_title", task.title.clone()),
            ("task_brief", task.brief.clone()),
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
        let cfg = &self.spec.workers;
        let req = LaunchRequest {
            harness: Some(worker.harness),
            cwd: cwd.clone(),
            model: cfg.model.clone(),
            permission: cfg.permission.clone(),
            title: Some(task.title.clone()),
            prompt: Some(self.worker_prompt(task, &worker.branch, None)),
            labels: BTreeMap::from([
                ("jefe_job".to_owned(), self.job.id.0.clone()),
                ("jefe_task".to_owned(), task.id.0.clone()),
            ]),
            launcher: cfg.launcher.clone(),
            worktree: Some(worker.worktree.clone()),
            sandbox: cfg.sandbox.clone(),
            ..LaunchRequest::default()
        };
        let task_id = task.id.clone();
        let rules = crate::policy::worker_rules(&cfg.policy);
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        worker.cwd = Some(cwd);
        match worker.launch(req, rules, now) {
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
            Obs::TurnDone => self.on_turn_done(w),
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
                    if let Some(exec) = worker.reject(&p) {
                        self.run(exec, Purpose::Input(w.clone()));
                    }
                    self.emit(Event::PermissionDenied {
                        worker: w.clone(),
                        title: p.title.clone(),
                    });
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
        let result = match parse_result(&message) {
            Some(r) => r,
            None if worker.result_nudges == 0 && worker.turns < max_turns => {
                worker.result_nudges += 1;
                self.prompt_worker(w, ASK_FOR_RESULT.to_owned(), false);
                return;
            }
            None => WorkerResult {
                status: ResultStatus::Done,
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
            ResultStatus::Done => self.run_diff(&task_id),
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
        let mut next = Worker::new(
            new_id.clone(),
            task.clone(),
            req.harness.unwrap_or(old.harness),
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
        let rules = crate::policy::worker_rules(&self.spec.workers.policy);
        let now = self.now;
        let Some(worker) = self.job.worker_mut(w) else {
            return;
        };
        let Some(req) = worker.pending_launch.take() else {
            return;
        };
        let exec = match worker.launch(req, rules, now) {
            Ok(exec) => exec,
            Err(e) => {
                self.start_failed(w, format!("can't launch: {e}"));
                return;
            }
        };
        let Some(worker) = self.job.worker(w) else { return };
        let (task, harness, branch) = (worker.task.clone(), worker.harness, worker.branch.clone());
        if let Some(task) = task {
            self.emit(Event::WorkerSpawned {
                worker: w.clone(),
                task,
                harness,
                branch,
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
                s.harness,
            );
            ids.push(id);
        }
        self.book.wrap_asked = false;
        ids
    }

    fn push_task(
        &mut self,
        id: TaskId,
        title: String,
        brief: String,
        kind: TaskKind,
        depends_on: Vec<TaskId>,
        harness: Option<acc_core::Harness>,
    ) {
        self.emit(Event::TaskAdded {
            task: id.clone(),
            title: title.clone(),
            kind: kind.clone(),
            depends_on: depends_on.clone(),
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
            harness,
            status: TaskStatus::Pending,
            attempts: 0,
            conflicts: 0,
            worker: None,
            result: None,
            diff: None,
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
        let max = self.spec.max_diff_chars;
        let diff = if stdout.len() > max {
            let cut = (0..=max).rev().find(|i| stdout.is_char_boundary(*i)).unwrap_or(0);
            format!("{}\n[diff cut at {max} characters]", &stdout[..cut])
        } else {
            stdout.to_owned()
        };
        let Some(t) = self.job.task_mut(task) else { return };
        if t.status != TaskStatus::Reported {
            return;
        }
        t.diff = Some(diff.clone());
        let (Some(worker), Some(result)) = (t.worker.clone(), t.result.clone()) else {
            return;
        };
        let trigger = Trigger::Review {
            task: task.clone(),
            worker,
        };
        if self.spec.budget.review_required {
            let focus = Focus::Review {
                task: task.clone(),
                result,
                diff,
            };
            self.consult(trigger, Some(focus));
        } else {
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
        }
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
            TaskKind::Implement => None,
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
                TaskKind::Implement => None,
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
        let harness = t.harness;
        // Its harness can go; the branch stays until the conflict task merges it.
        self.stop_task_workers(task);
        self.book.next_task += 1;
        let id = TaskId(format!("t{}", self.book.next_task));
        let kind = TaskKind::ResolveConflict {
            of: task.clone(),
            branch,
        };
        self.push_task(id, title, brief, kind, Vec::new(), harness);
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
        let situation = self.situation(&ask, &pending);
        self.emit(Event::Consulted {
            ask: ask.clone(),
            trigger: pending.trigger.clone(),
        });
        self.book.asks.insert(ask.clone(), pending);
        self.out.push(Effect::Consult {
            ask,
            situation: Box::new(situation),
        });
    }

    /// What the brain sees for `pending`.
    pub fn situation_for(&self, ask: &AskId) -> Option<Situation> {
        self.book.asks.get(ask).map(|p| self.situation(ask, p))
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
            limits: Limits {
                max_workers: b.max_workers,
                max_tasks: b.max_tasks,
                max_attempts: b.max_attempts,
                max_turns_per_worker: b.max_turns_per_worker,
            },
            tasks: self
                .job
                .tasks
                .iter()
                .map(|t| TaskView {
                    id: t.id.clone(),
                    title: t.title.clone(),
                    brief: t.brief.clone(),
                    kind: t.kind.clone(),
                    status: t.status,
                    attempts: t.attempts,
                    depends_on: t.depends_on.clone(),
                    worker: t.worker.clone(),
                    summary: t.result.as_ref().map(|r| r.summary.clone()),
                })
                .collect(),
            workers: self
                .job
                .workers
                .iter()
                .filter(|w| w.is_live() || pending.trigger.worker() == Some(&w.id))
                .map(|w| WorkerView {
                    id: w.id.clone(),
                    task: w.task.clone(),
                    harness: w.harness,
                    phase: w.phase.name().to_owned(),
                    turns: w.turns,
                    respawns: w.respawns,
                    stalls: w.stalls,
                    plan: w.plan.clone(),
                    cost_usd: w.usage.cost_usd,
                    tail: tail(&w.last_message().unwrap_or_default(), TAIL_CHARS),
                })
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

    fn apply(&mut self, pending: &PendingAsk, action: Action) {
        let trigger = &pending.trigger;
        match action {
            Action::Plan { tasks } => {
                let ids = self.add_tasks(&tasks, &[]);
                self.job.status = JobStatus::Running;
                self.emit(Event::Planned { tasks: ids });
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
                let branch = self.job.worker(&w).map(|x| x.branch.clone()).unwrap_or_default();
                let text = self.worker_prompt(t, &branch, Some(&feedback));
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
                let ask = self.next_ask();
                let questions = match &pending.focus {
                    Some(Focus::Question { questions, .. }) => questions.len(),
                    _ => 1,
                };
                self.book.human = Some(HumanAsk {
                    ask: ask.clone(),
                    trigger: trigger.clone(),
                    questions,
                });
                self.job.status = JobStatus::NeedsHuman;
                self.emit(Event::HumanAsked {
                    ask: ask.clone(),
                    question: question.clone(),
                    options: options.clone(),
                });
                self.out.push(Effect::AskHuman {
                    ask,
                    question,
                    options,
                });
            }
            Action::GiveUpTask { task, reason } => self.fail_task(&task, reason),
            Action::GiveUp { reason } => self.fail_job(reason),
            Action::Done { summary } => self.finish(JobStatus::Done, summary),
        }
    }

    // ---- the human ---------------------------------------------------------------------

    fn on_human(&mut self, reply: HumanReply) {
        match reply {
            HumanReply::Cancel => self.finish(JobStatus::Cancelled, "cancelled".to_owned()),
            HumanReply::Nudge { text } => {
                self.notice(format!("note from the human: {text}"));
                self.job.notes.push(text);
            }
            HumanReply::Answer { ask, text } => {
                let Some(h) = self.book.human.clone() else { return };
                if ask.as_ref().is_some_and(|a| *a != h.ask) {
                    return;
                }
                self.book.human = None;
                self.job.status = JobStatus::Running;
                self.emit(Event::HumanAnswered {
                    ask: h.ask.clone(),
                    text: text.clone(),
                });
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

    /// What a human's free-text answer means for the question it answers.
    fn human_action(&self, h: &HumanAsk, text: &str) -> Option<Action> {
        let trigger = &h.trigger;
        let yes = matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "y" | "yes" | "ok" | "accept" | "lgtm" | "merge"
        );
        match trigger {
            Trigger::Plan | Trigger::Wrap => None,
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
        let stalled: Vec<(WorkerId, u64)> = self
            .job
            .workers
            .iter()
            .filter(|w| w.is_live() && matches!(w.phase, WorkerPhase::Working | WorkerPhase::Waiting))
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
                let merged = w
                    .task
                    .as_ref()
                    .and_then(|t| self.job.task(t))
                    .is_some_and(|t| t.status == TaskStatus::Merged);
                if !merged {
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
        self.book.asks.clear();
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
