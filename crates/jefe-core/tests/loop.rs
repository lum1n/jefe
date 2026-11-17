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
    /// Log chunks for the orchestrator's session, one per turn.
    brain_script: VecDeque<Vec<u8>>,
    /// Exit codes for merges, in order; 0 when empty.
    merges: VecDeque<i32>,
    preflight: String,
    finished: Option<JobStatus>,
    runs: Vec<Purpose>,
    /// Each launch exec's script and stdin (worker and brain), in order.
    launched: Vec<String>,
    /// Each prompt to the brain, as the exec wrote it.
    brain_prompts: Vec<String>,
    /// What each input exec wrote to a worker.
    written: Vec<(WorkerId, String)>,
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
            brain_script: VecDeque::new(),
            merges: VecDeque::new(),
            preflight: "/repo\nmain\n".to_owned(),
            finished: None,
            runs: Vec::new(),
            launched: Vec::new(),
            brain_prompts: Vec::new(),
            written: Vec::new(),
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
                        Purpose::Launch(_) => {
                            self.launched.push(format!(
                                "{}\n{}",
                                exec.script,
                                String::from_utf8_lossy(&exec.stdin)
                            ));
                            self.done(id, 0, "4242\n")
                        }
                        Purpose::Input(w) => {
                            self.done(id, 0, "");
                            // A prompt, or an answer the harness was waiting on.
                            let stdin = String::from_utf8_lossy(&exec.stdin);
                            self.written.push((w.clone(), stdin.clone().into_owned()));
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
                        Purpose::BrainLaunch(_) => {
                            self.launched.push(format!(
                                "{}\n{}",
                                exec.script,
                                String::from_utf8_lossy(&exec.stdin)
                            ));
                            self.done(id, 0, "4343\n")
                        }
                        Purpose::BrainInput(b) => {
                            self.done(id, 0, "");
                            let stdin = String::from_utf8_lossy(&exec.stdin).into_owned();
                            if stdin.contains("\"type\":\"user\"") {
                                self.brain_prompts.push(stdin);
                                // The brain's replies aren't held: `hold_follow` is for workers.
                                self.deliver_brain(&b);
                            }
                        }
                        Purpose::BrainStatus(_) => self.done(id, 0, "alive 4343 0\n"),
                        Purpose::BrainSpecWrite(_)
                        | Purpose::BrainKill(_)
                        | Purpose::BrainRemove(_)
                        | Purpose::Save
                        | Purpose::Lease
                        | Purpose::Release => self.done(id, 0, ""),
                    }
                }
            }
        }
    }

    /// The brain session's next turn of output (scripted under key 0).
    fn deliver_brain(&mut self, id: &str) {
        let next = self.brain_script.pop_front();
        if let Some(bytes) = next {
            self.feed(Input::Bytes {
                worker: WorkerId::from(id),
                bytes,
            });
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
    // The brain reviews alone; the reviewer role has tests of its own.
    s.review_by = jefe_core::ReviewBy::Brain;
    s
}

/// A Claude stream-json turn: `text` as the assistant's message, with the
/// session's init line first when `first`.
fn claude_turn(text: &str, first: bool, cost: f64) -> Vec<u8> {
    let mut out = String::new();
    if first {
        out.push_str(
            "{\"type\": \"system\", \"subtype\": \"init\", \"session_id\": \"fake-1\", \"cwd\": \"/w\", \"model\": \"fake\", \"tools\": [], \"permissionMode\": \"acceptEdits\"}\n",
        );
    }
    let msg = serde_json::json!({
        "type": "assistant",
        "message": {"id": "msg_x", "type": "message", "role": "assistant", "model": "fake",
            "content": [{"type": "text", "text": text}]},
        "session_id": "fake-1"
    });
    out.push_str(&msg.to_string());
    out.push('\n');
    let res = serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false, "result": "ok", "session_id": "fake-1",
        "num_turns": 1, "total_cost_usd": cost, "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    out.push_str(&res.to_string());
    out.push('\n');
    out.into_bytes()
}

/// A worker's turn ending in a result block.
fn worker_turn(result: &str) -> Vec<u8> {
    claude_turn(
        &format!("Here you go.\n\n```jefe-result\n{result}\n```"),
        true,
        0.01,
    )
}

/// A brain's turn ending in a decision block.
fn brain_turn(decision: &str, first: bool) -> Vec<u8> {
    claude_turn(
        &format!("Thinking it over.\n\n```jefe-decision\n{decision}\n```"),
        first,
        0.05,
    )
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
            TaskSpec::new("Schema", "Add the table"),
            TaskSpec {
                depends_on: vec!["Schema".to_owned()],
                ..TaskSpec::new("API", "Expose it")
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
    s.approve_plan = Some(false);
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

/// Grants approvals; the rules decide everything else.
struct AllowApprovals;

impl Brain for AllowApprovals {
    fn decide(&mut self, s: &Situation) -> Decision {
        match &s.trigger {
            jefe_core::Trigger::Approval { worker, .. } => {
                assert!(
                    matches!(&s.focus, Some(jefe_core::brain::Focus::Approval { title, .. }) if !title.is_empty()),
                    "{:?}",
                    s.focus
                );
                assert!(
                    s.allowed.contains(&jefe_core::ActionKind::Allow),
                    "{:?}",
                    s.allowed
                );
                Decision {
                    action: Action::Allow {
                        worker: worker.clone(),
                    },
                    rationale: "scripted".to_owned(),
                    confidence: 0.9,
                    source: DecisionSource::Brain {
                        harness: None,
                        model: None,
                    },
                }
            }
            _ => RulesBrain.decide(s),
        }
    }
}

fn approval_job(unmatched: jefe_core::Unmatched, attended: bool, brain: Box<dyn Brain>) -> Host {
    let mut s = spec();
    s.workers.unmatched = unmatched;
    s.attended = attended;
    s.approve_plan = Some(false);
    Host::new(s, brain).script(
        1,
        vec![
            fixture("permission-deny-1.ndjson"),
            fixture("permission-deny-2.ndjson"),
            fixture("worker-result-only.ndjson"),
        ],
    )
}

#[test]
fn with_unmatched_brain_the_brain_grants_an_approval() {
    let mut h = approval_job(jefe_core::Unmatched::Brain, false, Box::new(AllowApprovals));
    h.drain();
    let kinds = h.kinds();
    assert!(kinds.iter().any(|k| k == "permission_granted"), "{kinds:?}");
    assert!(!kinds.iter().any(|k| k == "permission_denied"), "{kinds:?}");
    assert!(
        h.events.iter().any(|e| matches!(
            e,
            Event::Consulted {
                trigger: jefe_core::Trigger::Approval { .. },
                ..
            }
        )),
        "{kinds:?}"
    );
    assert_eq!(h.finished, Some(JobStatus::Done), "{kinds:?}");
}

#[test]
fn with_unmatched_brain_the_rules_still_deny() {
    let mut h = approval_job(jefe_core::Unmatched::Brain, false, Box::new(RulesBrain));
    h.drain();
    assert!(h.has("permission_denied"), "{:?}", h.kinds());
    assert!(!h.has("permission_granted"), "{:?}", h.kinds());
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

#[test]
fn with_unmatched_human_the_human_decides() {
    let mut h = approval_job(jefe_core::Unmatched::Human, true, Box::new(RulesBrain));
    h.drain();
    assert_eq!(h.human_asks.len(), 1, "{:?}", h.kinds());
    assert!(h.human_asks[0].contains("Allow it?"), "{:?}", h.human_asks);
    assert!(!h.events.iter().any(|e| matches!(
        e,
        Event::Consulted {
            trigger: jefe_core::Trigger::Approval { .. },
            ..
        }
    )));
    h.feed(Input::Human(HumanReply::Answer {
        ask: None,
        text: "allow".to_owned(),
    }));
    h.drain();
    assert!(h.has("permission_granted"), "{:?}", h.kinds());
}

#[test]
fn with_unmatched_human_and_nobody_there_it_is_denied() {
    let mut h = approval_job(jefe_core::Unmatched::Human, false, Box::new(RulesBrain));
    h.drain();
    assert!(h.human_asks.is_empty());
    assert!(h.has("permission_denied"), "{:?}", h.kinds());
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

// ---- roles, reports and reviewers -----------------------------------------------

fn roles_plan() -> Action {
    use jefe_core::job::TaskSpec;
    Action::Plan {
        tasks: vec![
            TaskSpec {
                role: "explorer".to_owned(),
                ..TaskSpec::new("Map the code", "Where does hello go?")
            },
            TaskSpec {
                depends_on: vec!["Map the code".to_owned()],
                ..TaskSpec::new("Add hello", "Add hello.txt")
            },
        ],
    }
}

#[test]
fn an_explorer_reports_and_its_report_reaches_the_task_that_depends_on_it() {
    let mut h = Host::new(spec(), Box::new(FakeBrain(VecDeque::from([roles_plan()]))))
        .script(
            1,
            vec![worker_turn(
                r#"{"status":"done","summary":"hello.txt belongs at the root","notes":""}"#,
            )],
        )
        .script(2, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let job = h.jefe.job();
    assert_eq!(job.tasks[0].status, TaskStatus::Done);
    assert!(
        job.tasks[0]
            .report
            .as_deref()
            .unwrap()
            .contains("belongs at the root")
    );
    assert_eq!(job.tasks[1].status, TaskStatus::Merged);
    assert!(
        h.events.iter().any(
            |e| matches!(e, Event::Artifact { task, role, .. } if task.as_str() == "t1" && role == "explorer")
        ),
        "{:?}",
        h.kinds()
    );
    // The explorer never went through diff or merge; its worktree was dropped.
    assert!(!h.runs.contains(&Purpose::Diff(jefe_core::TaskId::from("t1"))));
    assert!(h.runs.contains(&Purpose::Discard(w(1))));
    // The implementor's prompt carried the report; its session got the role label.
    let implementor = job.workers[1].session().unwrap();
    assert_eq!(
        implementor.spec().labels.get("jefe_role").map(String::as_str),
        Some("implementor")
    );
    let launch = h.runs.iter().filter(|p| matches!(p, Purpose::Launch(_))).count();
    assert_eq!(launch, 2);
    let explorer = &job.workers[0];
    assert_eq!(explorer.config.access, jefe_core::Access::ReadOnly);
    assert!(explorer.session().unwrap().spec().permission.is_none());
    match h
        .events
        .iter()
        .find(|e| matches!(e, Event::WorkerSpawned { role, .. } if role == "explorer"))
    {
        Some(Event::WorkerSpawned { task, .. }) => assert_eq!(task.as_str(), "t1"),
        e => panic!("{e:?}"),
    }
    match h.events.last() {
        Some(Event::JobDone { merged, branches, .. }) => {
            assert_eq!(merged.len(), 1);
            assert!(branches.is_empty(), "{branches:?}");
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn the_implementor_brief_holds_the_explorers_report() {
    // Hold the second worker's prompt so its text can be read.
    let mut h = Host::new(spec(), Box::new(FakeBrain(VecDeque::from([roles_plan()])))).script(
        1,
        vec![worker_turn(
            r#"{"status":"done","summary":"Put it at the root."}"#,
        )],
    );
    h.drain();
    let launched = h.jefe.job().workers[1].session().unwrap().spec().clone();
    assert_eq!(launched.labels.get("jefe_task").map(String::as_str), Some("t2"));
    // The first prompt is written right after the launch.
    let prompt_of = |n: u32| -> String {
        h.written
            .iter()
            .filter(|(id, _)| *id == w(n))
            .map(|(_, text)| text.clone())
            .next()
            .unwrap_or_else(|| panic!("nothing written to w{n}: {:?}", h.written))
    };
    assert!(prompt_of(1).contains("Your role: explorer"), "{}", prompt_of(1));
    assert!(prompt_of(2).contains("Put it at the root."), "{}", prompt_of(2));
    assert!(
        prompt_of(2).contains("Report from t1 (explorer)"),
        "{}",
        prompt_of(2)
    );
    assert!(
        prompt_of(2).contains("Your role: implementor"),
        "{}",
        prompt_of(2)
    );
    let task = h.jefe.job().task(&jefe_core::TaskId::from("t2")).unwrap();
    assert_eq!(task.depends_on, vec![jefe_core::TaskId::from("t1")]);
}

#[test]
fn a_reviewer_task_informs_the_brains_review() {
    let mut s = spec();
    s.review_by = jefe_core::ReviewBy::Both;
    let mut h = Host::new(s, Box::new(RulesBrain))
        .script(
            1,
            vec![
                fixture("worker-reports.ndjson"),
                fixture("worker-result-only.ndjson"),
            ],
        )
        .script(
            2,
            vec![worker_turn(
                r#"{"status":"done","verdict":"reject","summary":"No test covers hello.txt."}"#,
            )],
        )
        .script(
            3,
            vec![worker_turn(
                r#"{"status":"done","verdict":"accept","summary":"Fine now."}"#,
            )],
        );
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let job = h.jefe.job();
    assert_eq!(
        job.tasks.len(),
        3,
        "{:?}",
        job.tasks.iter().map(|t| &t.title).collect::<Vec<_>>()
    );
    assert!(matches!(job.tasks[1].kind, TaskKind::Review { ref of } if of.as_str() == "t1"));
    assert_eq!(job.tasks[1].role, "reviewer");
    assert_eq!(job.tasks[1].status, TaskStatus::Done);
    // The rules took the reviewer's rejection, the worker was prompted with it, then accepted.
    let reviews: Vec<(bool, Option<String>)> = h
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Reviewed {
                accepted, feedback, ..
            } => Some((*accepted, feedback.clone())),
            _ => None,
        })
        .collect();
    assert!(!reviews[0].0);
    assert!(
        reviews[0].1.as_deref().unwrap().contains("No test"),
        "{reviews:?}"
    );
    assert!(reviews[1].0);
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::WorkerPrompted { text, .. } if text.contains("No test covers"))),
        "{:?}",
        h.kinds()
    );
    assert_eq!(job.tasks[0].status, TaskStatus::Merged);
    // A reviewer may run beside a full house.
    assert_eq!(job.counters.spawns, 3);
}

#[test]
fn a_reviewer_alone_decides_when_asked_to() {
    let mut s = spec();
    s.review_by = jefe_core::ReviewBy::Reviewer;
    s.budget.max_workers = 1;
    let mut h = Host::new(s, Box::new(RulesBrain))
        .script(1, vec![fixture("worker-reports.ndjson")])
        .script(
            2,
            vec![worker_turn(
                r#"{"status":"done","verdict":"accept","summary":"Good."}"#,
            )],
        );
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let by_reviewer = h.events.iter().any(|e| {
        matches!(e, Event::Decided { decision, .. } if matches!(decision.source, DecisionSource::Reviewer { .. }))
    });
    assert!(by_reviewer, "{:?}", h.kinds());
    assert!(
        !h.events.iter().any(|e| matches!(
            e,
            Event::Consulted {
                trigger: jefe_core::Trigger::Review { .. },
                ..
            }
        )),
        "the brain was asked to review"
    );
}

#[test]
fn a_disabled_role_in_a_plan_is_refused() {
    use jefe_core::job::TaskSpec;
    let mut s = spec();
    s.roles.insert(
        "explorer".to_owned(),
        jefe_core::RoleConfig {
            enabled: false,
            ..s.role("explorer").unwrap()
        },
    );
    let bad = Action::Plan {
        tasks: vec![TaskSpec {
            role: "explorer".to_owned(),
            ..TaskSpec::new("Look", "")
        }],
    };
    let mut h = Host::new(s, Box::new(FakeBrain(VecDeque::from([bad]))))
        .script(1, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::DecisionRejected { why, .. } if why.contains("unknown role"))),
        "{:?}",
        h.kinds()
    );
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

// ---- plan approval ----------------------------------------------------------------

#[test]
fn an_attended_job_waits_for_the_plan_to_be_approved() {
    let mut s = spec();
    s.attended = true;
    let mut h = Host::new(s, Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert_eq!(h.jefe.job().status, JobStatus::AwaitingApproval);
    assert!(!h.has("worker_spawned"), "{:?}", h.kinds());
    assert_eq!(h.human_asks.len(), 1);
    assert!(
        h.human_asks[0].contains("t1 [implementor]"),
        "{}",
        h.human_asks[0]
    );
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::HumanAsked { kind: Some(k), .. } if k == "plan"))
    );
    // Feedback: the plan is dropped and made again with the note.
    h.feed(Input::Human(HumanReply::Answer {
        ask: None,
        text: "Split it in two".to_owned(),
    }));
    h.drain();
    assert_eq!(h.jefe.job().status, JobStatus::AwaitingApproval);
    assert!(h.has("task_dropped"));
    assert_eq!(h.jefe.job().notes, vec!["Split it in two".to_owned()]);
    assert_eq!(h.jefe.job().tasks.len(), 1, "revised plan replaces the old one");
    assert_eq!(h.jefe.job().tasks[0].id.as_str(), "t2");
    h.feed(Input::Human(HumanReply::Answer {
        ask: None,
        text: "approve".to_owned(),
    }));
    h.drain();
    assert!(h.has("plan_approved"));
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

#[test]
fn approval_can_be_turned_off_for_attended_jobs() {
    let mut s = spec();
    s.attended = true;
    s.approve_plan = Some(false);
    let mut h = Host::new(s, Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert!(h.human_asks.is_empty());
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

// ---- the orchestrator as a session --------------------------------------------------

fn session_spec() -> JobSpec {
    let mut s = spec();
    s.brain.kind = jefe_core::BrainKind::Session;
    s.brain.sandbox = false;
    s
}

#[test]
fn a_session_brain_is_consulted_turn_by_turn_with_deltas() {
    let mut h =
        Host::new(session_spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.brain_script = VecDeque::from([
        brain_turn(
            r#"{"action":"plan","tasks":[{"title":"Add hello","brief":"Add hello.txt"}],"rationale":"one file","confidence":0.9}"#,
            true,
        ),
        brain_turn(r#"{"action":"accept","task":"t1","rationale":"fine"}"#, false),
        brain_turn(r#"{"action":"done","summary":"hello landed"}"#, false),
    ]);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    // No Consult effect went to the host: the loop ran the brain itself.
    assert!(
        !h.has("consulted")
            || h.events.iter().all(
                |e| !matches!(e, Event::Decided { decision, .. } if decision.source == DecisionSource::Rules)
            )
    );
    let started = h.kinds().iter().filter(|k| *k == "brain_started").count();
    assert_eq!(started, 1, "{:?}", h.kinds());
    let turns = h.kinds().iter().filter(|k| *k == "brain_turn").count();
    assert_eq!(turns, 3);
    let by_brain: Vec<_> = h
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Decided { decision, .. } if matches!(decision.source, DecisionSource::Brain { .. }) => {
                Some(decision.action.kind())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        by_brain,
        [
            jefe_core::ActionKind::Plan,
            jefe_core::ActionKind::Accept,
            jefe_core::ActionKind::Done
        ]
    );
    // The first turn carried the whole situation, later ones the delta.
    assert_eq!(h.brain_prompts.len(), 3, "{:?}", h.brain_prompts);
    assert!(
        h.brain_prompts[0].contains("You are the brain of jefe"),
        "{}",
        h.brain_prompts[0]
    );
    assert!(
        h.brain_prompts[0].contains("Roles a task can have"),
        "{}",
        h.brain_prompts[0]
    );
    assert!(h.brain_prompts[0].contains("explorer"), "{}", h.brain_prompts[0]);
    assert!(
        h.brain_prompts[1..]
            .iter()
            .all(|t| t.contains("What changed since your last turn")),
        "{:?}",
        h.brain_prompts
    );
    assert!(h.brain_prompts[1].contains("review"), "{}", h.brain_prompts[1]);
    assert!(h.brain_prompts[1].contains("reported"), "{}", h.brain_prompts[1]);
    assert!(h.has("brain_ended"), "retired at the end: {:?}", h.kinds());
    let job = h.jefe.job();
    assert!(job.brain_cost_usd > 0.0, "{}", job.brain_cost_usd);
    let brain = h.jefe.brain_session().unwrap();
    assert_eq!(brain.id, "jefe-j1-b1");
    assert_eq!(brain.turns, 3);
}

#[test]
fn a_session_brain_rolls_over_after_its_turns_and_a_oneshot_brain_every_time() {
    for (kind, max_turns, sessions) in [
        (jefe_core::BrainKind::Session, 2, 2),
        (jefe_core::BrainKind::Oneshot, 40, 3),
    ] {
        let mut s = session_spec();
        s.brain.kind = kind;
        s.brain.max_turns = max_turns;
        let mut h = Host::new(s, Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
        let first_again = kind == jefe_core::BrainKind::Oneshot;
        h.brain_script = VecDeque::from([
            brain_turn(
                r#"{"action":"plan","tasks":[{"title":"Add hello","brief":"Add hello.txt"}]}"#,
                true,
            ),
            brain_turn(r#"{"action":"accept","task":"t1"}"#, first_again),
            brain_turn(r#"{"action":"done","summary":"hello landed"}"#, true),
        ]);
        h.drain();
        assert_eq!(h.finished, Some(JobStatus::Done), "{kind:?}: {:?}", h.kinds());
        let started = h.kinds().iter().filter(|k| *k == "brain_started").count();
        assert_eq!(started, sessions, "{kind:?}: {:?}", h.kinds());
    }
}

#[test]
fn a_brain_without_a_block_is_asked_once_then_the_rules_decide() {
    let mut h =
        Host::new(session_spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.brain_script = VecDeque::from([
        claude_turn("I think one task is right.", true, 0.01),
        claude_turn("Still no block, sorry.", false, 0.01),
    ]);
    h.drain();
    let asked_again = h.brain_prompts.iter().filter(|t| t.contains("no valid")).count();
    assert_eq!(asked_again, 1, "{:?}", h.brain_prompts);
    assert!(
        notices(&h).iter().any(|n| n.contains("the brain failed")),
        "{:?}",
        notices(&h)
    );
    // The brain's session stays for the next ask; the plan came from the rules.
    assert!(h.events.iter().any(
        |e| matches!(e, Event::Decided { decision, .. } if decision.source == DecisionSource::Rules
                && decision.action.kind() == jefe_core::ActionKind::Plan)
    ));
    // Later asks go to the brain again, which never answers: each times out
    // and the rules decide, until the job is done.
    let timeout = h.jefe.spec().budget.brain_timeout_ms;
    for _ in 0..4 {
        if h.finished.is_some() {
            break;
        }
        h.now += timeout + 1;
        h.feed(Input::Tick);
        h.drain();
    }
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
}

#[test]
fn a_brain_that_times_out_is_retired_and_the_rules_decide() {
    let mut s = session_spec();
    s.budget.brain_timeout_ms = 1_000;
    let mut h = Host::new(s, Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    // No brain script: it never replies.
    h.drain();
    assert_eq!(h.jefe.job().status, JobStatus::Planning);
    let due = h.jefe.next_deadline().unwrap();
    assert!(due <= h.now + 1_000, "{due} vs {}", h.now);
    h.now = due + 1;
    h.feed(Input::Tick);
    h.drain();
    assert!(h.has("brain_ended"), "{:?}", h.kinds());
    assert!(h.runs.iter().any(|p| matches!(p, Purpose::BrainKill(_))));
    // The plan came from the rules; the next ask starts a fresh brain.
    assert!(h.has("planned"));
    let started = h.kinds().iter().filter(|k| *k == "brain_started").count();
    assert!(started >= 2, "{:?}", h.kinds());
}

#[test]
fn a_message_with_consult_gives_the_brain_a_turn_of_its_own() {
    let mut h = Host::new(session_spec(), Box::new(RulesBrain));
    h.brain_script = VecDeque::from([
        brain_turn(
            r#"{"action":"plan","tasks":[{"title":"Add hello","brief":"Add hello.txt"}]}"#,
            true,
        ),
        brain_turn(
            r#"{"action":"prompt","worker":"jefe-j1-w1","text":"Also add a newline."}"#,
            false,
        ),
    ]);
    h.drain();
    h.feed(Input::Human(HumanReply::Message {
        text: "Make sure the file ends with a newline".to_owned(),
        consult: true,
    }));
    h.drain();
    assert!(
        h.events.iter().any(|e| matches!(
            e,
            Event::Consulted {
                trigger: jefe_core::Trigger::Nudge,
                ..
            }
        )),
        "{:?}",
        h.kinds()
    );
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::WorkerPrompted { text, .. } if text.contains("newline"))),
        "{:?}",
        h.kinds()
    );
    assert_eq!(h.jefe.job().notes.len(), 1);
}

#[test]
fn a_session_brain_survives_a_restart() {
    let mut h =
        Host::new(session_spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.brain_script = VecDeque::from([
        brain_turn(
            r#"{"action":"plan","tasks":[{"title":"Add hello","brief":"Add hello.txt"}]}"#,
            true,
        ),
        brain_turn(r#"{"action":"accept","task":"t1"}"#, false),
        brain_turn(r#"{"action":"done","summary":"hello landed"}"#, false),
    ]);
    h.hold_follow = true;
    h.drain();
    assert_eq!(h.jefe.job().status, JobStatus::Running);
    let snap = h.jefe.snapshot();
    let worker_spec = h.jefe.job().workers[0].session().unwrap().spec().clone();
    let brain_spec = h.jefe.brain_session().unwrap().spec().unwrap().clone();
    let specs = vec![worker_spec, brain_spec];
    let (jefe, effects) = Jefe::restore(&snap, &specs, h.now).unwrap();
    h.jefe = jefe;
    h.queue = effects.into();
    h.hold_follow = false;
    h.drain();
    assert!(
        h.runs.iter().any(|p| matches!(p, Purpose::BrainStatus(_))),
        "{:?}",
        h.runs
    );
    h.deliver(&w(1));
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done), "{:?}", h.kinds());
    let started = h.kinds().iter().filter(|k| *k == "brain_started").count();
    assert_eq!(started, 1, "the same session went on: {:?}", h.kinds());
}

// ---- the cost cap ------------------------------------------------------------------------

#[test]
fn the_cost_cap_ends_the_job() {
    let mut s = spec();
    s.budget.max_cost_usd = Some(0.001);
    let mut h = Host::new(s, Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Failed), "{:?}", h.kinds());
    assert!(
        h.events
            .iter()
            .any(|e| matches!(e, Event::JobFailed { reason, .. } if reason.contains("cap"))),
        "{:?}",
        h.kinds()
    );
}

// ---- hosted ------------------------------------------------------------------------------

#[test]
fn opening_a_finished_job_releases_its_lease_at_once() {
    use jefe_core::Hosted;
    let mut h = Host::new(spec(), Box::new(RulesBrain)).script(1, vec![fixture("worker-reports.ndjson")]);
    h.drain();
    assert_eq!(h.finished, Some(JobStatus::Done));
    let snap = h.jefe.snapshot();
    let (hosted, effects) = Hosted::open(&snap, &[], "phone", 9_000).unwrap();
    let purposes: Vec<Purpose> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Run { purpose, .. } => Some(purpose.clone()),
            _ => None,
        })
        .collect();
    assert!(purposes.contains(&Purpose::Save), "{purposes:?}");
    assert!(
        !purposes.contains(&Purpose::Lease),
        "nothing to renew: {purposes:?}"
    );
    let _ = hosted;
}

#[test]
fn a_hosted_job_saves_and_renews_its_lease_and_stops_when_it_is_taken() {
    use jefe_core::Hosted;
    let (mut hosted, effects) = Hosted::start(spec(), "desk", 1_000);
    let mut purposes: Vec<Purpose> = Vec::new();
    let mut queue: VecDeque<Effect> = effects.into();
    let mut now = 1_000;
    let mut lease_holder = "desk".to_owned();
    let mut saves = 0;
    let mut taken_over = false;
    let mut events: Vec<Event> = Vec::new();
    let mut steps = 0;
    while let Some(effect) = queue.pop_front() {
        steps += 1;
        assert!(steps < 500, "{purposes:?}");
        match effect {
            Effect::Run {
                id, purpose, exec, ..
            } => {
                purposes.push(purpose.clone());
                let (code, stdout) = match &purpose {
                    Purpose::Save => {
                        saves += 1;
                        let text = String::from_utf8_lossy(&exec.stdin).into_owned();
                        let mut lines = text.lines();
                        assert!(lines.next().unwrap().contains("\"v\":1"), "{text}");
                        if saves == 1 {
                            assert!(lines.next().unwrap().contains("job_created"), "{text}");
                        }
                        (0, String::new())
                    }
                    Purpose::Lease => (
                        0,
                        format!(
                            "{}\t{lease_holder}\t1\t30\n",
                            if lease_holder == "desk" { "ok" } else { "held" }
                        ),
                    ),
                    Purpose::Preflight => (0, "/repo\nmain\n".to_owned()),
                    _ => (0, String::new()),
                };
                now += 10;
                queue.extend(hosted.step(
                    Input::ExecDone {
                        id,
                        code,
                        stdout,
                        stderr: String::new(),
                    },
                    now,
                ));
            }
            Effect::Consult { ask, situation } => {
                let decision = RulesBrain.decide(&situation);
                now += 10;
                queue.extend(hosted.step(Input::Decided { ask, decision }, now));
            }
            Effect::Emit(s) => events.push(s.event),
            Effect::WakeIn { ms } if queue.is_empty() => {
                now += ms;
                queue.extend(hosted.step(Input::Tick, now));
            }
            _ => {}
        }
        if queue.is_empty() && saves >= 1 && !taken_over {
            // A renewal comes due; by then another client holds the lease.
            taken_over = true;
            lease_holder = "phone".to_owned();
            now = hosted.next_deadline().unwrap().max(now + 1);
            queue.extend(hosted.step(Input::Tick, now));
        }
    }
    assert!(saves >= 1, "{purposes:?}");
    assert!(
        purposes.iter().filter(|p| **p == Purpose::Lease).count() >= 2,
        "{purposes:?}"
    );
    assert!(hosted.lease_lost());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::LeaseLost { holder } if holder == "phone")),
        "{events:?}"
    );
    // Nothing more comes out once the lease is gone.
    assert!(hosted.step(Input::Tick, now + 60_000).is_empty());
}
