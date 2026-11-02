//! The host side of the loop: runs each [`Effect`] the core returns and feeds
//! back what happened, persisting the job after every step.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use acc_ssh::Host;
use acc_ssh::{Detached, Read};
use jefe_core::{Brain, Effect, HumanReply, Input, Jefe, JobStatus, Report, RulesBrain, WorkerId};
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::brain::HarnessBrain;
use crate::output::{Mode, Output};
use crate::store::Store;

/// Longest wait between ticks, even with no deadline due.
const HEARTBEAT_MS: u64 = 30_000;
/// Shortest, so a deadline that doesn't move can't spin.
const MIN_SLEEP_MS: u64 = 200;
/// Pause before following a log again after its reader gave up.
const FOLLOW_RETRY: Duration = Duration::from_secs(2);

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Steers a running [`Supervisor`] from the program that embeds it: the
/// stdin control commands as calls. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Control(mpsc::UnboundedSender<Msg>);

impl Control {
    /// A handle that steers nothing (for a job that is already over).
    pub(crate) fn detached() -> Self {
        Self(mpsc::unbounded_channel().0)
    }

    /// Answers the open `ask` (or whatever is open when `None`).
    pub fn answer(&self, ask: Option<jefe_core::AskId>, text: &str) {
        self.human(HumanReply::Answer {
            ask,
            text: text.to_owned(),
        });
    }

    /// A note for the brain.
    pub fn nudge(&self, text: &str) {
        self.human(HumanReply::Nudge {
            text: text.to_owned(),
        });
    }

    /// Stops the job, keeping every branch.
    pub fn cancel(&self) {
        self.human(HumanReply::Cancel);
    }

    /// Asks for a [`crate::output::Message::Snapshot`].
    pub fn status(&self) {
        let _ = self.0.send(Msg::Status);
    }

    fn human(&self, reply: HumanReply) {
        let _ = self.0.send(Msg::Input(Input::Human(reply)));
    }
}

#[derive(Debug)]
enum Msg {
    Input(Input),
    /// `{"cmd":"status"}`: print a snapshot.
    Status,
}

/// Who answers the loop's consultations.
#[derive(Debug, Clone)]
pub enum BrainRunner {
    Rules,
    Harness(Arc<HarnessBrain>),
}

/// Drives one job to its end.
#[derive(Debug)]
pub struct Supervisor {
    jefe: Jefe,
    host: Arc<Host>,
    store: Store,
    out: Output,
    brain: BrainRunner,
    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    followers: HashMap<WorkerId, JoinHandle<()>>,
    wake_at: Option<u64>,
    report: Option<Report>,
    /// Ctrl-C and SIGTERM cancel the job (not when embedded).
    signals: bool,
}

impl Supervisor {
    pub fn new(jefe: Jefe, host: Arc<Host>, store: Store, out: Output, brain: BrainRunner) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            jefe,
            host,
            store,
            out,
            brain,
            tx,
            rx,
            followers: HashMap::new(),
            wake_at: None,
            report: None,
            signals: true,
        }
    }

    /// Leaves signals to the embedding program.
    pub fn without_signals(mut self) -> Self {
        self.signals = false;
        self
    }

    pub fn control(&self) -> Control {
        Control(self.tx.clone())
    }

    /// Runs `effects` (from [`Jefe::new`] or [`Jefe::restore`]), then the
    /// loop until the job finishes. `first` is fed before anything else.
    pub async fn run(
        mut self,
        effects: Vec<Effect>,
        first: Vec<Input>,
        control: bool,
    ) -> anyhow::Result<Report> {
        self.out.hello();
        self.store.save(&self.jefe.snapshot())?;
        self.apply(effects)?;
        for input in first {
            self.step(input)?;
        }
        if control {
            self.read_control();
        }
        if self.signals {
            self.watch_signals();
        }
        while self.report.is_none() {
            let now = now_ms();
            let due = [self.jefe.next_deadline(), self.wake_at]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(u64::MAX)
                .min(now + HEARTBEAT_MS);
            let sleep = Duration::from_millis(due.saturating_sub(now).max(MIN_SLEEP_MS));
            tokio::select! {
                msg = self.rx.recv() => match msg {
                    Some(Msg::Input(input)) => {
                        let input = self.with_option(input);
                        self.step(input)?
                    }
                    Some(Msg::Status) => {
                        let snap: serde_json::Value = serde_json::from_str(&self.jefe.snapshot())?;
                        self.out.snapshot(&snap["job"]);
                    }
                    None => break,
                },
                () = tokio::time::sleep(sleep) => {
                    if self.wake_at.is_some_and(|w| w <= now_ms()) {
                        self.wake_at = None;
                    }
                    self.step(Input::Tick)?;
                }
            }
        }
        for (_, f) in self.followers.drain() {
            f.abort();
        }
        self.store.release();
        let report = self
            .report
            .ok_or_else(|| anyhow::anyhow!("the loop stopped before the job finished"))?;
        Ok(report)
    }

    fn step(&mut self, input: Input) -> anyhow::Result<()> {
        let now = now_ms();
        self.store.input(&input, now)?;
        let effects = self.jefe.step(input, now);
        self.store.save(&self.jefe.snapshot())?;
        self.apply(effects)
    }

    /// In prose an answer may be an option's number.
    fn with_option(&self, input: Input) -> Input {
        match input {
            Input::Human(HumanReply::Answer { ask, text }) if self.out.mode == Mode::Prose => {
                Input::Human(HumanReply::Answer {
                    ask,
                    text: self.out.answer_text(&text),
                })
            }
            other => other,
        }
    }

    fn send(&self, input: Input) {
        let _ = self.tx.send(Msg::Input(input));
    }

    fn apply(&mut self, effects: Vec<Effect>) -> anyhow::Result<()> {
        for effect in effects {
            match effect {
                Effect::Run { id, exec, .. } => {
                    let (host, tx) = (self.host.clone(), self.tx.clone());
                    tokio::spawn(async move {
                        let input = match host.exec(&exec.script, &exec.stdin).await {
                            Ok(out) => Input::ExecDone {
                                id,
                                code: out.code.map_or(-1, |c| c as i32),
                                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                            },
                            Err(e) => Input::ExecDone {
                                id,
                                code: -1,
                                stdout: String::new(),
                                stderr: e.to_string(),
                            },
                        };
                        let _ = tx.send(Msg::Input(input));
                    });
                }
                Effect::Follow { worker, offset, .. } => self.follow(worker, offset),
                Effect::Unfollow { worker } => {
                    if let Some(f) = self.followers.remove(&worker) {
                        f.abort();
                    }
                }
                Effect::Consult { ask, situation } => match &self.brain {
                    BrainRunner::Rules => {
                        let decision = RulesBrain.decide(&situation);
                        self.send(Input::Decided { ask, decision });
                    }
                    BrainRunner::Harness(brain) => {
                        let (brain, host, tx) = (brain.clone(), self.host.clone(), self.tx.clone());
                        let log_dir = Some(self.store.dir().join("brain"));
                        tokio::spawn(async move {
                            let input = match brain.consult(host, &ask, &situation, log_dir).await {
                                Ok(decision) => Input::Decided { ask, decision },
                                Err(why) => Input::BrainFailed { ask, why },
                            };
                            let _ = tx.send(Msg::Input(input));
                        });
                    }
                },
                Effect::AskHuman {
                    ask,
                    question,
                    options,
                } => self.out.ask(&ask, &question, &options),
                Effect::Emit(s) => {
                    self.store.event(&s)?;
                    if let jefe_core::Event::WorkerReported { worker, .. } = &s.event
                        && let Some(text) = self.jefe.job().worker(worker).and_then(|w| w.last_message())
                    {
                        self.store.worker_final(worker.as_str(), &text)?;
                    }
                    self.out.event(&s);
                }
                Effect::WakeIn { ms } => {
                    let at = now_ms().saturating_add(ms);
                    self.wake_at = Some(self.wake_at.map_or(at, |w| w.min(at)));
                }
                Effect::Finished { report, .. } => {
                    self.store.report(&report)?;
                    self.out.finished(&report);
                    self.report = Some(report);
                }
            }
        }
        Ok(())
    }

    /// Streams a worker's log into the loop, resuming at the exact offset
    /// after dropped connections.
    fn follow(&mut self, worker: WorkerId, offset: u64) {
        if let Some(old) = self.followers.remove(&worker) {
            old.abort();
        }
        let (host, tx, id) = (self.host.clone(), self.tx.clone(), worker.clone());
        let task = tokio::spawn(async move {
            let d = match Detached::attach(host, id.as_str(), offset) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("jefe: can't follow {id}: {e}");
                    let _ = tx.send(Msg::Input(Input::LogEnded {
                        worker: id,
                        code: None,
                    }));
                    return;
                }
            };
            loop {
                match d.read().await {
                    Ok(Read::Data(bytes)) => {
                        let input = Input::Bytes {
                            worker: id.clone(),
                            bytes,
                        };
                        if tx.send(Msg::Input(input)).is_err() {
                            return;
                        }
                    }
                    Ok(Read::Exited(code)) => {
                        let _ = tx.send(Msg::Input(Input::LogEnded { worker: id, code }));
                        return;
                    }
                    Err(e) => {
                        eprintln!("jefe: following {id}: {e}; retrying");
                        tokio::time::sleep(FOLLOW_RETRY).await;
                    }
                }
            }
        });
        self.followers.insert(worker, task);
    }

    /// Human input on stdin: prose lines, or `{"cmd":...}` in JSON mode.
    fn read_control(&self) {
        let (tx, mode) = (self.tx.clone(), self.out.mode);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let msg = match mode {
                    Mode::Json => match parse_control(line) {
                        Ok(m) => m,
                        Err(e) => {
                            crate::output::json_error("bad_control", &format!("bad control line: {e}"));
                            continue;
                        }
                    },
                    Mode::Prose if line == "q" => Msg::Input(Input::Human(HumanReply::Cancel)),
                    Mode::Prose => Msg::Input(Input::Human(HumanReply::Answer {
                        ask: None,
                        text: line.to_owned(),
                    })),
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });
    }

    /// Ctrl-C or SIGTERM (`jefe stop`) cancels the job, keeping branches; a
    /// second one exits at once.
    fn watch_signals(&self) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                return;
            };
            let mut cancelled = false;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                if cancelled {
                    std::process::exit(crate::output::exit_code(JobStatus::Cancelled));
                }
                cancelled = true;
                let _ = tx.send(Msg::Input(Input::Human(HumanReply::Cancel)));
            }
        });
    }
}

fn parse_control(line: &str) -> Result<Msg, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(line)?;
    if v.get("cmd").and_then(|c| c.as_str()) == Some("status") {
        return Ok(Msg::Status);
    }
    Ok(Msg::Input(Input::Human(serde_json::from_value(v)?)))
}
