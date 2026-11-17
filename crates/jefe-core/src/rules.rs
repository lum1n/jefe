//! The transition table: which actions a trigger allows, and whether a
//! decision fits the job as it is now. A brain only proposes; this disposes.

use crate::brain::{Action, ActionKind, Trigger};
use crate::ids::{TaskId, WorkerId};
use crate::job::{Budget, Job, JobSpec, REVIEWER_ROLE, TaskKind, TaskStatus};
use crate::result::ResultStatus;
use crate::worker::WorkerPhase;
use acc_core::BlockKind;

/// The longest a brain may ask jefe to wait.
pub const MAX_WAIT_MS: u64 = 60 * 60_000;

/// What the job's configuration adds to the table.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub spec: &'a JobSpec,
}

impl Context<'_> {
    fn budget(&self) -> &Budget {
        &self.spec.budget
    }
}

fn base(trigger: &Trigger) -> &'static [ActionKind] {
    use ActionKind::*;
    match trigger {
        Trigger::Plan => &[Plan, AskHuman, GiveUp],
        Trigger::Report { .. } => &[Prompt, Split, Handoff, AskHuman, GiveUpTask, GiveUp],
        Trigger::Review { .. } => &[Accept, Reject, AskHuman, GiveUpTask],
        Trigger::Question { .. } => &[Answer, AskHuman, Interrupt, GiveUpTask],
        Trigger::Approval { .. } => &[Allow, Deny, AskHuman, GiveUpTask],
        Trigger::WorkerError { .. } => &[Prompt, Respawn, Handoff, GiveUpTask, GiveUp],
        Trigger::Exited { .. } => &[Respawn, Handoff, GiveUpTask, GiveUp],
        Trigger::Auth { .. } => &[AskHuman, Handoff, GiveUpTask, GiveUp],
        Trigger::Stalled { .. } => &[Wait, Interrupt, Prompt, Respawn, Handoff, GiveUpTask],
        Trigger::Quota { .. } => &[Handoff, Wait, GiveUpTask],
        Trigger::Wrap => &[Done, Plan, GiveUp],
        Trigger::Nudge => &[
            Wait, Plan, Prompt, Split, Interrupt, AskHuman, GiveUpTask, GiveUp, Done,
        ],
    }
}

/// The actions `trigger` allows now.
pub fn allowed(job: &Job, trigger: &Trigger, cx: Context<'_>) -> Vec<ActionKind> {
    let worker = trigger.worker().and_then(|w| job.worker(w));
    let task = worker.and_then(|w| w.task.as_ref()).and_then(|t| job.task(t));
    let b = cx.budget();
    base(trigger)
        .iter()
        .copied()
        .filter(|k| match k {
            ActionKind::AskHuman => cx.spec.attended,
            ActionKind::Respawn => task.is_some_and(|t| t.attempts < b.max_attempts),
            ActionKind::Handoff => {
                task.is_some_and(|t| t.attempts < b.max_attempts)
                    && worker.is_some_and(|w| w.config.alternates.iter().any(|h| *h != w.harness))
            }
            ActionKind::Prompt if trigger.worker().is_some() => {
                worker.is_some_and(|w| w.turns < b.max_turns_per_worker)
            }
            ActionKind::Plan if matches!(trigger, Trigger::Wrap | Trigger::Nudge) => {
                (job.tasks.len() as u32) < b.max_tasks
            }
            ActionKind::Done if matches!(trigger, Trigger::Nudge) => {
                job.tasks.iter().all(|t| t.status.is_terminal())
            }
            _ => true,
        })
        .collect()
}

/// Whether `action` may happen now. `Err` says why not, for the brain's next try.
pub fn validate(job: &Job, allowed: &[ActionKind], action: &Action, cx: Context<'_>) -> Result<(), String> {
    if !allowed.contains(&action.kind()) {
        let names: Vec<_> = allowed.iter().map(|k| k.name()).collect();
        return Err(format!(
            "{} is not allowed now; choose one of: {}",
            action.kind().name(),
            names.join(", ")
        ));
    }
    let b = cx.budget();
    match action {
        Action::Plan { tasks } => {
            if tasks.is_empty() {
                return Err("a plan needs at least one task".to_owned());
            }
            if job.tasks.len() + tasks.len() > b.max_tasks as usize {
                return Err(format!("a job has at most {} tasks", b.max_tasks));
            }
            check_specs(job, tasks, cx.spec)
        }
        Action::Split { task, into } => {
            let t = task_in(job, task)?;
            if !matches!(t.status, TaskStatus::Pending | TaskStatus::Reported) {
                return Err(format!("{task} can't be split while {}", status_name(t.status)));
            }
            if into.len() < 2 {
                return Err("split into at least two tasks".to_owned());
            }
            if job.tasks.len() + into.len() > b.max_tasks as usize {
                return Err(format!("a job has at most {} tasks", b.max_tasks));
            }
            check_specs(job, into, cx.spec)
        }
        Action::Prompt { worker, text } => {
            let w = live_worker(job, worker)?;
            if text.trim().is_empty() {
                return Err("the prompt is empty".to_owned());
            }
            if w.turns >= b.max_turns_per_worker {
                return Err(format!("{worker} has used its {} turns", b.max_turns_per_worker));
            }
            Ok(())
        }
        Action::Answer { worker, answers } => {
            let w = live_worker(job, worker)?;
            if !matches!(
                w.phase,
                WorkerPhase::Blocked {
                    kind: BlockKind::Question,
                    ..
                }
            ) {
                return Err(format!("{worker} has no open question"));
            }
            if answers.is_empty() || answers.iter().any(|a| a.iter().all(|s| s.trim().is_empty())) {
                return Err("give an answer to every question".to_owned());
            }
            Ok(())
        }
        Action::Allow { worker } | Action::Deny { worker } => {
            let w = live_worker(job, worker)?;
            if !matches!(
                w.phase,
                WorkerPhase::Blocked {
                    kind: BlockKind::Permission,
                    ..
                }
            ) {
                return Err(format!("{worker} is not waiting for an approval"));
            }
            Ok(())
        }
        Action::Interrupt { worker } => live_worker(job, worker).map(|_| ()),
        Action::Handoff { worker, harness, .. } => {
            let w = job.worker(worker).ok_or_else(|| format!("no worker {worker}"))?;
            if w.harness == *harness {
                return Err(format!("{worker} already runs {harness:?}; respawn instead"));
            }
            replaceable(job, worker, b)
        }
        Action::Respawn { worker } => replaceable(job, worker, b),
        Action::Accept { task } => {
            let t = task_in(job, task)?;
            let done = t.result.as_ref().is_some_and(|r| r.status == ResultStatus::Done);
            if t.status != TaskStatus::Reported || !done || t.diff.is_none() {
                return Err(format!("{task} has no finished work to accept"));
            }
            if reports(job, t) {
                return Err(format!("{task} is a report task ({}); nothing to merge", t.role));
            }
            Ok(())
        }
        Action::Reject { task, feedback } => {
            let t = task_in(job, task)?;
            if t.status != TaskStatus::Reported {
                return Err(format!("{task} is not waiting for review"));
            }
            if feedback.trim().is_empty() {
                return Err("say what to change".to_owned());
            }
            let worker = t.worker.as_ref().ok_or_else(|| format!("{task} has no worker"))?;
            let w = live_worker(job, worker)?;
            if w.turns >= b.max_turns_per_worker {
                return Err(format!("{worker} has used its {} turns", b.max_turns_per_worker));
            }
            Ok(())
        }
        Action::Wait { ms } => {
            if *ms == 0 || *ms > MAX_WAIT_MS {
                return Err(format!("wait between 1 and {MAX_WAIT_MS} ms"));
            }
            Ok(())
        }
        Action::AskHuman { question, .. } => {
            if question.trim().is_empty() {
                return Err("the question is empty".to_owned());
            }
            Ok(())
        }
        Action::GiveUpTask { task, .. } => {
            let t = task_in(job, task)?;
            if t.status.is_terminal() {
                return Err(format!("{task} is already {}", status_name(t.status)));
            }
            Ok(())
        }
        Action::GiveUp { .. } | Action::Done { .. } => Ok(()),
    }
}

/// The task ends with a report, not a merge: its worker's role says so,
/// else its role in the spec.
fn reports(job: &Job, t: &crate::job::Task) -> bool {
    matches!(t.kind, TaskKind::Review { .. })
        || t.worker
            .as_ref()
            .and_then(|w| job.worker(w))
            .map(|w| w.config.access.reports())
            .unwrap_or(false)
}

fn task_in<'a>(job: &'a Job, id: &TaskId) -> Result<&'a crate::job::Task, String> {
    job.task(id).ok_or_else(|| format!("no task {id}"))
}

fn live_worker<'a>(job: &'a Job, id: &WorkerId) -> Result<&'a crate::worker::Worker, String> {
    let w = job.worker(id).ok_or_else(|| format!("no worker {id}"))?;
    if !w.is_live() || !w.has_session() {
        return Err(format!("{id} is not running"));
    }
    Ok(w)
}

fn replaceable(job: &Job, id: &WorkerId, b: &Budget) -> Result<(), String> {
    let w = job.worker(id).ok_or_else(|| format!("no worker {id}"))?;
    if w.replaced_by.is_some() || matches!(w.phase, WorkerPhase::Gone | WorkerPhase::Discarding) {
        return Err(format!("{id} is finished"));
    }
    if !w.has_session() {
        return Err(format!("{id} never started"));
    }
    let task = w
        .task
        .as_ref()
        .and_then(|t| job.task(t))
        .ok_or_else(|| format!("{id} has no task"))?;
    if task.status.is_terminal() {
        return Err(format!("{} is already {}", task.id, status_name(task.status)));
    }
    if task.attempts >= b.max_attempts {
        return Err(format!("{} has used its {} attempts", task.id, b.max_attempts));
    }
    Ok(())
}

fn check_specs(job: &Job, specs: &[crate::job::TaskSpec], spec: &JobSpec) -> Result<(), String> {
    let roles = spec.enabled_roles();
    for (i, s) in specs.iter().enumerate() {
        if s.title.trim().is_empty() {
            return Err("every task needs a title".to_owned());
        }
        if s.role == REVIEWER_ROLE {
            return Err(format!(
                "{:?}: the machine adds {REVIEWER_ROLE} tasks itself; plan the work and it gets reviewed",
                s.title
            ));
        }
        if !roles.contains_key(&s.role) {
            let known: Vec<&str> = roles.keys().map(String::as_str).collect();
            return Err(format!(
                "{:?} has unknown role {:?}; use one of: {}",
                s.title,
                s.role,
                known.join(", ")
            ));
        }
        for dep in &s.depends_on {
            let earlier = specs[..i].iter().any(|o| &o.title == dep);
            let existing = job.tasks.iter().any(|t| t.id.as_str() == dep || &t.title == dep);
            if !earlier && !existing {
                return Err(format!("{:?} depends on unknown task {dep:?}", s.title));
            }
        }
    }
    Ok(())
}

pub fn status_name(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Pending => "pending",
        TaskStatus::Assigned => "assigned",
        TaskStatus::Working => "working",
        TaskStatus::Reported => "reported",
        TaskStatus::Accepted => "accepted",
        TaskStatus::Conflicted => "conflicted",
        TaskStatus::Merged => "merged",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Dropped => "dropped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::rules::ALL_KINDS;
    use crate::ids::{HostId, JobId};
    use crate::job::{Counters, JobStatus, Task, TaskKind, TaskSpec};
    use crate::worker::Worker;
    use acc_core::Harness;

    fn spec() -> JobSpec {
        let mut s = JobSpec::new("j", "g", "/r");
        s.workers.alternates = vec![Harness::Codex];
        s
    }

    fn job(spec: &JobSpec) -> Job {
        let w = WorkerId::from("jefe-j-w1");
        let t = TaskId::from("t1");
        let cfg = spec
            .resolve("implementor")
            .unwrap_or_else(|| panic!("no implementor"));
        Job {
            id: JobId::from("j"),
            goal: "g".to_owned(),
            repo: "/r".to_owned(),
            base: Some("main".to_owned()),
            host: HostId::local(),
            status: JobStatus::Running,
            tasks: vec![Task {
                id: t.clone(),
                title: "T".to_owned(),
                brief: String::new(),
                kind: TaskKind::Implement,
                depends_on: Vec::new(),
                role: "implementor".to_owned(),
                harness: None,
                model: None,
                status: TaskStatus::Working,
                attempts: 1,
                conflicts: 0,
                worker: Some(w.clone()),
                result: None,
                diff: None,
                report: None,
                commit: None,
                failure: None,
            }],
            workers: vec![Worker::new(w, t, cfg, HostId::local(), "jefe-j-w1".to_owned(), 0)],
            counters: Counters::default(),
            created_ms: 0,
            notes: Vec::new(),
            brain_cost_usd: 0.0,
            report: None,
        }
    }

    fn sample(kind: ActionKind) -> Action {
        let w = || WorkerId::from("jefe-j-w1");
        let t = || TaskId::from("t1");
        let spec = || TaskSpec::new("x", "");
        match kind {
            ActionKind::Plan => Action::Plan { tasks: vec![spec()] },
            ActionKind::Split => Action::Split {
                task: t(),
                into: vec![spec(), spec()],
            },
            ActionKind::Prompt => Action::Prompt {
                worker: w(),
                text: "go".to_owned(),
            },
            ActionKind::Answer => Action::Answer {
                worker: w(),
                answers: vec![vec!["a".to_owned()]],
            },
            ActionKind::Allow => Action::Allow { worker: w() },
            ActionKind::Deny => Action::Deny { worker: w() },
            ActionKind::Interrupt => Action::Interrupt { worker: w() },
            ActionKind::Handoff => Action::Handoff {
                worker: w(),
                harness: Harness::Codex,
                prompt: None,
            },
            ActionKind::Respawn => Action::Respawn { worker: w() },
            ActionKind::Accept => Action::Accept { task: t() },
            ActionKind::Reject => Action::Reject {
                task: t(),
                feedback: "no".to_owned(),
            },
            ActionKind::Wait => Action::Wait { ms: 1_000 },
            ActionKind::AskHuman => Action::AskHuman {
                question: "?".to_owned(),
                options: Vec::new(),
            },
            ActionKind::GiveUpTask => Action::GiveUpTask {
                task: t(),
                reason: "r".to_owned(),
            },
            ActionKind::GiveUp => Action::GiveUp {
                reason: "r".to_owned(),
            },
            ActionKind::Done => Action::Done {
                summary: "s".to_owned(),
            },
        }
    }

    fn triggers() -> Vec<Trigger> {
        let w = || WorkerId::from("jefe-j-w1");
        let t = || TaskId::from("t1");
        vec![
            Trigger::Plan,
            Trigger::Report {
                task: t(),
                worker: w(),
            },
            Trigger::Review {
                task: t(),
                worker: w(),
            },
            Trigger::Question {
                worker: w(),
                request_id: "r".to_owned(),
            },
            Trigger::Approval {
                worker: w(),
                request_id: "r".to_owned(),
            },
            Trigger::WorkerError { worker: w() },
            Trigger::Exited { worker: w() },
            Trigger::Auth { worker: w() },
            Trigger::Stalled { worker: w() },
            Trigger::Quota { worker: w() },
            Trigger::Wrap,
            Trigger::Nudge,
        ]
    }

    #[test]
    fn every_out_of_table_action_is_refused() {
        for attended in [false, true] {
            let mut spec = spec();
            spec.attended = attended;
            let job = job(&spec);
            let cx = Context { spec: &spec };
            for trigger in triggers() {
                let allowed = allowed(&job, &trigger, cx);
                assert!(allowed.iter().all(|k| base(&trigger).contains(k)));
                for kind in ALL_KINDS {
                    if !allowed.contains(kind) {
                        let err = validate(&job, &allowed, &sample(*kind), cx);
                        assert!(err.is_err(), "{kind:?} passed for {trigger:?}");
                    }
                }
                if !attended {
                    assert!(!allowed.contains(&ActionKind::AskHuman));
                }
            }
        }
    }

    #[test]
    fn in_table_actions_are_checked_against_the_job() {
        let mut spec = spec();
        spec.workers.alternates = Vec::new();
        let job = job(&spec);
        let cx = Context { spec: &spec };
        let all = ALL_KINDS.to_vec();
        // Nothing reported yet.
        assert!(validate(&job, &all, &sample(ActionKind::Accept), cx).is_err());
        // No question open, and the worker never started.
        assert!(validate(&job, &all, &sample(ActionKind::Answer), cx).is_err());
        assert!(validate(&job, &all, &sample(ActionKind::Prompt), cx).is_err());
        assert!(validate(&job, &all, &Action::Wait { ms: 0 }, cx).is_err());
        let unknown_dep = Action::Plan {
            tasks: vec![TaskSpec {
                depends_on: vec!["nope".to_owned()],
                ..TaskSpec::new("x", "")
            }],
        };
        assert!(validate(&job, &all, &unknown_dep, cx).is_err());
        assert!(validate(&job, &all, &sample(ActionKind::Plan), cx).is_ok());
        assert!(validate(&job, &all, &sample(ActionKind::GiveUpTask), cx).is_ok());
        // Handoff to the harness it already runs.
        let same = Action::Handoff {
            worker: WorkerId::from("jefe-j-w1"),
            harness: Harness::Claude,
            prompt: None,
        };
        assert!(validate(&job, &all, &same, cx).is_err());
    }

    #[test]
    fn plans_may_only_use_enabled_roles_and_never_the_reviewer() {
        let mut spec = spec();
        spec.roles.insert(
            "researcher".to_owned(),
            crate::job::RoleConfig {
                enabled: false,
                ..spec.role("researcher").unwrap_or_default()
            },
        );
        let job = job(&spec);
        let cx = Context { spec: &spec };
        let all = ALL_KINDS.to_vec();
        let plan = |role: &str| Action::Plan {
            tasks: vec![TaskSpec {
                role: role.to_owned(),
                ..TaskSpec::new("x", "")
            }],
        };
        assert!(validate(&job, &all, &plan("explorer"), cx).is_ok());
        assert!(validate(&job, &all, &plan("tester"), cx).is_ok());
        let err = validate(&job, &all, &plan("researcher"), cx).unwrap_err();
        assert!(err.contains("unknown role"), "{err}");
        let err = validate(&job, &all, &plan("reviewer"), cx).unwrap_err();
        assert!(err.contains("machine adds"), "{err}");
        assert!(validate(&job, &all, &plan("janitor"), cx).is_err());
    }

    #[test]
    fn a_nudge_allows_done_only_when_every_task_is_over() {
        let spec = spec();
        let mut job = job(&spec);
        let cx = Context { spec: &spec };
        assert!(!allowed(&job, &Trigger::Nudge, cx).contains(&ActionKind::Done));
        job.tasks[0].status = TaskStatus::Merged;
        assert!(allowed(&job, &Trigger::Nudge, cx).contains(&ActionKind::Done));
        assert!(allowed(&job, &Trigger::Nudge, cx).contains(&ActionKind::Plan));
    }
}
