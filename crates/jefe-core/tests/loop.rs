//! The supervision loop driven end to end by a scripted host: no processes,
//! no clock, recorded and synthetic Claude streams as worker output.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, VecDeque};

use jefe_core::brain::{Action, Brain, Decision, DecisionSource, RulesBrain, Situation};
use jefe_core::job::{JobSpec, JobStatus, TaskKind, TaskStatus};
use jefe_core::{Effect, Event, HumanReply, Input, Jefe, Purpose, WorkerId};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/claude/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A brain that says what it is told, then falls back to the rules.
struct FakeBrain(VecDeque<Action>);

impl Brain for FakeBrain {
    fn decide(&mut self, s: &Situation) -> Decision {
        match self.0.pop_front() {
            Some(action) => Decision {
                action,
                rationale: "scripted".to_owned(),
                confidence: 0.9,
                source: DecisionSource::Brain {
                    harness: None,
                    model: None,
                },
            },
            None => RulesBrain.decide(s),
        }
    }
}

/// Plays the host: runs effects against scripted results.
struct Host {
    jefe: Jefe,
    brain: Box<dyn Brain>,
    now: u64,
    queue: VecDeque<Effect>,
    events: Vec<Event>,
    /// Every input fed, with its time, for replay.
    inputs: Vec<(Input, u64)>,
    /// Log chunks per worker number (1-based), one per prompt it gets.
    scripts: BTreeMap<u32, VecDeque<Vec<u8>>>,
    /// Exit codes for merges, in order; 0 when empty.
    merges: VecDeque<i32>,
    preflight: String,
    finished: Option<JobStatus>,
    runs: Vec<Purpose>,
    human_asks: Vec<String>,
    /// Stop delivering effects (to snapshot mid-job).
    hold_follow: bool,
    /// Execs to leave running (never answered), to snapshot while they are.
    hold: fn(&Purpose) -> bool,
    held: Vec<Purpose>,
}

fn worker_no(w: &WorkerId) -> u32 {
    w.as_str()
        .rsplit('w')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

impl Host {
    fn new(spec: JobSpec, brain: Box<dyn Brain>) -> Self {
        let (jefe, effects) = Jefe::new(spec, 1_000);
        Self {
            jefe,
            brain,
            now: 1_000,
            queue: effects.into(),
            events: Vec::new(),
            inputs: Vec::new(),
            scripts: BTreeMap::new(),
            merges: VecDeque::new(),
            preflight: "/repo\nmain\n".to_owned(),
            finished: None,
            runs: Vec::new(),
            human_asks: Vec::new(),
            hold_follow: false,
            hold: |_| false,
            held: Vec::new(),
        }
    }

    fn script(mut self, worker: u32, chunks: Vec<Vec<u8>>) -> Self {
        self.scripts.insert(worker, chunks.into());
        self
    }

    fn feed(&mut self, input: Input) {
        self.now += 10;
        self.inputs.push((input.clone(), self.now));
        let out = self.jefe.step(input, self.now);
        self.queue.extend(out);
    }

    fn done(&mut self, id: jefe_core::ExecId, code: i32, stdout: &str) {
        self.feed(Input::ExecDone {
            id,
            code,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        });
    }

    /// Runs effects until none are left.
    fn drain(&mut self) {
        while let Some(effect) = self.queue.pop_front() {
            match effect {
                Effect::Emit(s) => self.events.push(s.event),
                Effect::Finished { status, .. } => self.finished = Some(status),
                Effect::Consult { ask, situation } => {
                    let decision = self.brain.decide(&situation);
                    self.feed(Input::Decided { ask, decision });
                }
                Effect::AskHuman { question, .. } => self.human_asks.push(question),
                Effect::Follow { .. } | Effect::Unfollow { .. } | Effect::WakeIn { .. } => {}
                Effect::Run {
                    id, exec, purpose, ..
                } => {
                    self.runs.push(purpose.clone());
                    if (self.hold)(&purpose) {
                        self.held.push(purpose);
                        continue;
                    }
                    match purpose {
                        Purpose::Preflight => {
                            let out = self.preflight.clone();
                            self.done(id, 0, &out)
                        }
                        Purpose::WorktreeAdd(w) => self.done(id, 0, &format!("/home/u/.acc/w/{w}\n")),
                        Purpose::Launch(_) => self.done(id, 0, "4242\n"),
                        Purpose::Input(w) => {
                            self.done(id, 0, "");
                            // A prompt, or an answer the harness was waiting on.
                            let stdin = String::from_utf8_lossy(&exec.stdin);
                            let prompt =
                                stdin.contains("\"type\":\"user\"") || stdin.contains("control_response");
                            if prompt && !self.hold_follow {
                                self.deliver(&w);
                            }
                        }
                        Purpose::Diff(_) => self.done(id, 0, "diff --git a/hello.txt b/hello.txt\n+hello\n"),
                        Purpose::Merge(_) => {
                            let code = self.merges.pop_front().unwrap_or(0);
                            self.done(id, code, if code == 0 { "c0ffee\n" } else { "" })
                        }
                        Purpose::ConflictFiles(_) => self.done(id, 0, "hello.txt\n"),
                        Purpose::Status(_) => self.done(id, 0, "alive 4242 0\n"),
                        Purpose::SpecWrite(_)
                        | Purpose::Kill(_)
                        | Purpose::Remove(_)
                        | Purpose::Discard(_) => self.done(id, 0, ""),
                    }
                }
            }
        }
    }

    /// The worker's next turn of output.
    fn deliver(&mut self, w: &WorkerId) {
        let next = self.scripts.get_mut(&worker_no(w)).and_then(VecDeque::pop_front);
        if let Some(bytes) = next {
            self.feed(Input::Bytes {
                worker: w.clone(),
                bytes,
            });
        }
    }

    fn kinds(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    fn has(&self, kind: &str) -> bool {
        self.kinds().iter().any(|k| k == kind)
    }
}

fn spec() -> JobSpec {
    let mut s = JobSpec::new("j1", "Add a hello file", "/repo");
    s.workers.alternates = vec![acc_core::Harness::Codex];
    s
}

#[test]
fn one_worker_from_plan_to_done_on_the_rules() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let kinds = h.kinds();
    let order = [
        "job_created",
        "consulted",
        "planned",
        "worker_spawned",
        "worker_reported",
        "reviewed",
        "merged",
        "worker_discarded",
        "job_done",
    ];
    let mut at = 0;
    for k in order {
        let found = kinds[at..].iter().position(|x| x == k);
        assert!(found.is_some(), "{k} missing after {at} in {kinds:?}");
        at += found.unwrap();
    }
    let job = h.jefe.job();
    assert_eq!(job.tasks[0].status, TaskStatus::Merged);
    assert_eq!(job.tasks[0].commit.as_deref(), Some("c0ffee"));
    assert_eq!(job.base.as_deref(), Some("main"));
    // The exec order a real host sees.
    let runs: Vec<&str> = h
        .runs
        .iter()
        .map(|p| match p {
            Purpose::Preflight => "preflight",
            Purpose::WorktreeAdd(_) => "worktree_add",
            Purpose::Launch(_) => "launch",
            Purpose::Input(_) => "input",
            Purpose::Diff(_) => "diff",
            Purpose::Merge(_) => "merge",
            Purpose::Discard(_) => "discard",
            Purpose::Remove(_) => "remove",
            _ => "other",
        })
        .filter(|r| *r != "other")
        .collect();
    let want = [
        "preflight",
        "worktree_add",
        "launch",
        "input",
        "diff",
        "merge",
        "discard",
        "remove",
    ];
    let mut at = 0;
    for r in want {
        at += runs[at..]
            .iter()
            .position(|x| *x == r)
            .unwrap_or_else(|| panic!("{r} missing: {runs:?}"));
    }
    match h.events.last() {
        Some(Event::JobDone {
            summary,
            merged,
            branches,
        }) => {
            assert!(summary.contains("Added hello.txt"), "{summary}");
            assert_eq!(merged.len(), 1);
            assert!(branches.is_empty(), "{branches:?}");
        }
        e => panic!("last event {e:?}"),
    }
}

#[test]
fn a_dirty_checkout_fails_before_any_worker() {
    let mut h = Host::new(spec(), Box::new(RulesBrain));
    h.preflight = "/repo\nmain\n M src/lib.rs\n".to_owned();
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Failed));
    assert!(!h.has("worker_spawned"));
}

#[test]
fn the_wrong_base_fails() {
    let mut s = spec();
    s.base = Some("release".to_owned());
    let mut h = Host::new(s, Box::new(RulesBrain));
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Failed));
}

#[test]
fn a_refused_decision_is_retried_once_then_the_rules_decide() {
    let bogus = || Action::Accept {
        task: jefe_core::TaskId::from("t9"),
    };
    let brain = FakeBrain(VecDeque::from([bogus(), bogus()]));
    let mut h = Host::new(spec(), Box::new(brain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    let rejected = h.kinds().iter().filter(|k| *k == "decision_rejected").count();
    assert_eq!(rejected, 2);
    assert_eq!(h.finished, Some(JobStatus::Done));
    // The second try saw the first refusal.
    let consults = h.kinds().iter().filter(|k| *k == "consulted").count();
    assert!(consults >= 2);
}

#[test]
fn the_brain_plans_two_tasks_with_a_dependency() {
    use jefe_core::job::TaskSpec;
    let plan = Action::Plan {
        tasks: vec![
            TaskSpec {
                title: "Schema".to_owned(),
                brief: "Add the table".to_owned(),
                depends_on: vec![],
                harness: None,
            },
            TaskSpec {
                title: "API".to_owned(),
                brief: "Expose it".to_owned(),
                depends_on: vec!["Schema".to_owned()],
                harness: None,
            },
        ],
    };
    let mut h = Host::new(spec(), Box::new(FakeBrain(VecDeque::from([plan]))))
        .script(1, vec![fixture("worker-reports.ndjson")])
        .script(2, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let job = h.jefe.job();
    assert_eq!(job.tasks.len(), 2);
    assert!(job.tasks.iter().all(|t| t.status == TaskStatus::Merged));
    // t2 started only after t1 merged.
    let kinds = h.kinds();
    let first_merge = kinds.iter().position(|k| k == "merged").unwrap();
    let second_spawn = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| *k == "worker_spawned")
        .nth(1)
        .unwrap()
        .0;
    assert!(second_spawn > first_merge, "{kinds:?}");
}

#[test]
fn unmatched_approvals_are_denied_and_a_missing_result_is_asked_for() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(
        1,
        vec![
            fixture("permission-deny-1.ndjson"),
            fixture("permission-deny-2.ndjson"),
            fixture("worker-result-only.ndjson"),
        ],
    );
    h.drain();
    assert!(h.has("permission_denied"), "{:?}", h.kinds());
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::WorkerPrompted { text, .. } if text.contains("jefe-result")))
    );
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

#[test]
fn a_second_missing_result_counts_as_done() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(
        1,
        vec![
            fixture("worker-no-result.ndjson"),
            fixture("worker-still-no-result.ndjson"),
        ],
    );
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let notes = h.events.iter().find_map(|e| match e {
        Event::WorkerReported { result, .. } => Some(result.notes.clone()),
        _ => None,
    });
    assert!(notes.unwrap().contains("no result block"));
}

#[test]
fn questions_go_to_the_brain_and_get_answered() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-asks.ndjson")]);
    h.drain();
    let answered = h.events.iter().find_map(|e| match e {
        Event::WorkerAnswered { answers, .. } => Some(answers.clone()),
        _ => None,
    });
    assert_eq!(
        answered,
        Some(vec![vec!["Postgres".to_owned()]]),
        "{:?}",
        h.kinds()
    );
}

#[test]
fn an_attended_question_goes_to_the_human() {
    let mut s = spec();
    s.attended = true;
    let mut h = Host::new(s, Box::new(RulesBrain)).script(1, vec![fixture("worker-asks.ndjson")]);
    h.drain();
    assert_eq!(h.human_asks.len(), 1, "{:?}", h.kinds());
    assert!(h.human_asks[0].contains("Which DB?"));
    assert_eq!(h.jefe.job().status, JobStatus::NeedsHuman);
    h.feed(Input::Human(HumanReply::Answer {
        ask: None,
        text: "SQLite".to_owned(),
    }));
    h.drain();
    let answered = h.events.iter().find_map(|e| match e {
        Event::WorkerAnswered { answers, .. } => Some(answers.clone()),
        _ => None,
    });
    assert_eq!(answered, Some(vec![vec!["SQLite".to_owned()]]));
}

#[test]
fn a_silent_worker_stalls_and_the_rules_wait() {
    let mut s = spec();
    s.budget.stall_ms = 1_000;
    let mut h = Host::new(s, Box::new(RulesBrain));
    h.drain();
    h.now += 5_000;
    h.feed(Input::Tick);
    h.drain();
    assert!(h.has("worker_stalled"), "{:?}", h.kinds());
    let waited = h.events.iter().any(
        |e| matches!(e, Event::Decided { decision, .. } if matches!(decision.action, Action::Wait { .. })),
    );
    assert!(waited, "{:?}", h.kinds());
}

#[test]
fn the_wall_clock_ends_the_job() {
    let mut s = spec();
    s.budget.wall_ms = 1_000;
    let mut h = Host::new(s, Box::new(RulesBrain));
    h.drain();
    h.now += 5_000;
    h.feed(Input::Tick);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Failed));
    // The worker's branch is kept for a human.
    match h
        .events
        .iter()
        .rev()
        .find(|e| matches!(e, Event::JobFailed { .. }))
    {
        Some(Event::JobFailed { branches, .. }) => assert_eq!(branches, &["acc/jefe-j1-w1".to_owned()]),
        e => panic!("{e:?}"),
    }
}

#[test]
fn cancel_keeps_branches() {
    let mut h = Host::new(spec(), Box::new(RulesBrain));
    h.drain();
    h.feed(Input::Human(HumanReply::Cancel));
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Cancelled));
    assert!(!h.runs.iter().any(|p| matches!(p, Purpose::Discard(_))));
}

#[test]
fn a_conflict_becomes_a_task_that_merges_both() {
    let mut h = Host::new(spec(), Box::new(RulesBrain))
        .script(1, vec![fixture("worker-reports.ndjson")])
        .script(2, vec![fixture("worker-reports.ndjson")]);
    h.merges = VecDeque::from([7, 0]);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let job = h.jefe.job();
    assert_eq!(job.tasks.len(), 2);
    assert!(matches!(job.tasks[1].kind, TaskKind::ResolveConflict { .. }));
    assert!(job.tasks.iter().all(|t| t.status == TaskStatus::Merged));
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::MergeConflict { files, .. } if files == &["hello.txt".to_owned()]))
    );
}

#[test]
fn a_worker_that_exits_early_is_respawned_once() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(2, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    h.feed(Input::LogEnded {
        worker: WorkerId::from("jefe-j1-w1"),
        code: Some(1),
    });
    h.drain();
    assert!(h.has("worker_exited"));
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::WorkerReplaced { handoff: false, .. }))
    );
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

#[test]
fn replaying_the_inputs_yields_the_same_events() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(
        1,
        vec![
            fixture("permission-deny-1.ndjson"),
            fixture("permission-deny-2.ndjson"),
            fixture("worker-result-only.ndjson"),
        ],
    );
    h.drain();
    let (mut again, first) = Jefe::new(spec(), 1_000);
    let mut events: Vec<Event> = first
        .into_iter()
        .filter_map(|e| match e {
            Effect::Emit(s) => Some(s.event),
            _ => None,
        })
        .collect();
    for (input, now) in h.inputs.clone() {
        for e in again.step(input, now) {
            if let Effect::Emit(s) = e {
                events.push(s.event);
            }
        }
    }
    assert_eq!(events, h.events);
}

#[test]
fn a_snapshot_restores_and_the_job_finishes() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.hold_follow = true;
    h.drain();
    assert_eq!(h.jefe.job().status, JobStatus::Running);
    let snap = h.jefe.snapshot();
    let session_spec = h.jefe.job().workers[0].session().unwrap().spec().clone();

    let (jefe, effects) = Jefe::restore(&snap, &[session_spec], h.now).unwrap();
    h.jefe = jefe;
    h.queue = effects.into();
    h.hold_follow = false;
    h.drain();
    assert!(h.runs.iter().any(|p| matches!(p, Purpose::Status(_))));
    // The log the worker wrote while jefe was away.
    h.deliver(&WorkerId::from("jefe-j1-w1"));
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

/// Restores from `h`'s current snapshot with the sessions in `specs`.
fn restart(h: &mut Host, specs: &[acc_core::session::SessionSpec]) {
    let snap = h.jefe.snapshot();
    let (jefe, effects) = Jefe::restore(&snap, specs, h.now).unwrap();
    h.jefe = jefe;
    h.queue = effects.into();
    h.hold = |_| false;
    h.runs.clear();
}

fn notices(h: &Host) -> Vec<String> {
    h.events
        .iter()
        .filter_map(|e| match e {
            Event::Notice { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn w(n: u32) -> WorkerId {
    WorkerId::from(format!("jefe-j1-w{n}").as_str())
}

/// jefe died while writing the first prompt: after a silent while it is
/// written again, and the job goes on.
#[test]
fn a_prompt_lost_in_a_restart_is_sent_again() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.hold = |p| matches!(p, Purpose::Input(_));
    h.drain();
    assert_eq!(h.held, [Purpose::Input(w(1))]);
    let session_spec = h.jefe.job().workers[0].session().unwrap().spec().clone();
    restart(&mut h, &[session_spec]);
    h.drain();
    assert!(
        !h.runs.contains(&Purpose::Input(w(1))),
        "sent at once: {:?}",
        h.runs
    );
    let due = h.jefe.next_deadline().unwrap();
    assert!(
        due <= h.now + jefe_core::RESEND_AFTER_MS,
        "deadline {due} is too late"
    );
    h.now = due;
    h.feed(Input::Tick);
    h.drain();
    assert!(h.runs.contains(&Purpose::Input(w(1))), "{:?}", h.runs);
    assert!(notices(&h).iter().any(|n| n.contains("silent since the restart")));
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

/// The prompt did arrive before jefe died: the worker's output shows it,
/// and nothing is written twice.
#[test]
fn a_prompt_that_arrived_is_not_sent_twice() {
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.hold = |p| matches!(p, Purpose::Input(_));
    h.drain();
    let session_spec = h.jefe.job().workers[0].session().unwrap().spec().clone();
    restart(&mut h, &[session_spec]);
    h.drain();
    h.deliver(&w(1));
    h.drain();
    h.now += jefe_core::RESEND_AFTER_MS * 2;
    h.feed(Input::Tick);
    h.drain();
    assert!(!h.runs.contains(&Purpose::Input(w(1))), "{:?}", h.runs);
    assert!(!notices(&h).iter().any(|n| n.contains("silent since the restart")));
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

/// Respawns a stalled worker; the rules decide everything else.
struct RespawnOnStall;

impl Brain for RespawnOnStall {
    fn decide(&mut self, s: &Situation) -> Decision {
        match &s.trigger {
            jefe_core::Trigger::Stalled { worker } => Decision {
                action: Action::Respawn {
                    worker: worker.clone(),
                },
                rationale: "scripted".to_owned(),
                confidence: 0.9,
                source: DecisionSource::Brain {
                    harness: None,
                    model: None,
                },
            },
            _ => RulesBrain.decide(s),
        }
    }
}

/// A job whose stalled w1 is being replaced by w2, with w1's kill still
/// running. Returns w1's session spec.
fn replacing_a_stalled_worker() -> (Host, acc_core::session::SessionSpec) {
    let mut s = spec();
    s.budget.stall_ms = 1_000;
    let mut h = Host::new(s, Box::new(RespawnOnStall)).script(2, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    let old_spec = h.jefe.job().workers[0].session().unwrap().spec().clone();
    h.hold = |p| matches!(p, Purpose::Kill(_));
    h.now += 5_000;
    h.feed(Input::Tick);
    h.drain();
    assert_eq!(h.held, [Purpose::Kill(w(1))], "{:?}", h.kinds());
    assert!(h.jefe.job().workers[1].pending_launch.is_some());
    (h, old_spec)
}

/// jefe died while a respawn waited for the old worker to stop: the stop
/// runs again and the replacement still launches.
#[test]
fn a_replacement_waiting_on_a_kill_survives_a_restart() {
    let (mut h, old_spec) = replacing_a_stalled_worker();
    restart(&mut h, &[old_spec]);
    h.drain();
    let launches = h.runs.iter().filter(|p| **p == Purpose::Launch(w(2))).count();
    assert!(h.runs.contains(&Purpose::Kill(w(1))), "{:?}", h.runs);
    assert_eq!(launches, 1, "{:?}", h.runs);
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

/// The same, when the old worker's session is gone too: nothing is left
/// to stop, so the replacement launches straight away.
#[test]
fn a_replacement_launches_when_its_predecessor_is_gone() {
    let (mut h, _) = replacing_a_stalled_worker();
    restart(&mut h, &[]);
    h.drain();
    assert!(h.runs.contains(&Purpose::Launch(w(2))), "{:?}", h.runs);
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}
