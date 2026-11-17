//! The orchestrator as an acc session the loop launches and drives itself:
//! read-only, labelled `jefe_role=orchestrator`, consulted turn by turn.
//! With `BrainKind::Session` one session lasts the whole job (until
//! `max_turns`); with `Oneshot` each consultation gets a fresh one. acc
//! calls for the brain live here, as the workers' do in `worker.rs`.

use std::collections::BTreeMap;

use acc_core::command::Command;
use acc_core::event::ItemKind;
use acc_core::policy::{Decision as PolicyDecision, Policy};
use acc_core::sandbox::{Preset, Sandbox};
use acc_core::session::{LaunchRequest, Outbound, Permission, Session, SessionError, SessionSpec, Update};
use acc_core::{BlockKind, Event as AccEvent, Exec, Harness, RunState};
use serde::{Deserialize, Serialize};

use crate::ids::{AskId, JobId};
use crate::job::{BrainSpec, ORCHESTRATOR_ROLE};
use crate::worker::{CLIENT_TAG, Deferred};

/// What the brain is told when its reply had no decision block.
pub const ASK_FOR_BLOCK: &str = "Your reply had no valid ```jefe-decision block. Reply again with a short rationale \
     and exactly one ```jefe-decision block holding one of the allowed actions.";
/// What the brain's own questions get: nobody answers them.
pub const DECIDE_YOURSELF: &str =
    "Nobody can answer questions here: decide yourself and say what you assumed.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum BrainPhase {
    /// The harness is starting (its first prompt may be held for effort).
    Launching,
    /// Idle between consultations.
    Ready,
    /// A turn is running for `ask`.
    Thinking { ask: AskId, since_ms: u64 },
    /// Retired, exited or lost. A new session takes over at the next ask.
    Gone,
}

/// What a batch of acc updates meant for the loop.
#[derive(Debug, Clone, PartialEq)]
pub enum BrainObs {
    TurnDone,
    TurnError(String),
    Auth(String),
    Exited(Option<i32>),
}

/// One orchestrator session. This is the only module besides `worker.rs`
/// that drives `acc_core::session::Session`.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrainSession {
    /// `jefe-<job>-b<n>`.
    pub id: String,
    pub harness: Harness,
    pub model: Option<String>,
    pub phase: BrainPhase,
    /// Prompts sent.
    pub turns: u32,
    /// The current ask was already asked again for its missing block.
    pub retried: bool,
    /// Requests already answered.
    pub handled: Vec<String>,
    /// The first prompt, waiting for the effort levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<Deferred>,
    /// What this session spent, as last reported.
    pub cost_usd: f64,
    /// [`Session::snapshot`] as of the job's last snapshot.
    pub session_snapshot: Option<String>,
    #[serde(skip)]
    session: Option<Session>,
}

impl BrainSession {
    /// The launch request for session `id` in `repo`: read-only, with
    /// `prompt` as the first turn. The policy is set at launch.
    pub fn request(
        job: &JobId,
        spec: &BrainSpec,
        default_harness: Harness,
        repo: &str,
        prompt: String,
    ) -> LaunchRequest {
        let harness = spec.harness.unwrap_or(default_harness);
        LaunchRequest {
            harness: Some(harness),
            cwd: repo.to_owned(),
            model: spec.model.clone(),
            permission: spec.permission.clone(),
            title: Some(format!("jefe orchestrator for {job}")),
            // Held back when an effort is wanted; see `launch`.
            prompt: spec.effort.is_none().then_some(prompt),
            labels: BTreeMap::from([
                ("jefe_job".to_owned(), job.0.clone()),
                ("jefe_role".to_owned(), ORCHESTRATOR_ROLE.to_owned()),
            ]),
            launcher: spec.launcher.clone(),
            sandbox: spec.sandbox.then(|| Sandbox {
                preset: Preset::ReadOnly,
                ..Sandbox::default()
            }),
            ..LaunchRequest::default()
        }
    }

    /// Starts the session for `ask`. Run the returned exec on the host.
    pub fn launch(
        id: String,
        spec: &BrainSpec,
        req: LaunchRequest,
        first_prompt: String,
        ask: AskId,
        now_ms: u64,
    ) -> Result<(Self, Exec), SessionError> {
        let (mut session, exec) = Session::launch(req, &id, CLIENT_TAG, now_ms)?;
        session.set_policy(Some(Policy {
            rules: crate::policy::brain_rules(),
        }));
        let deferred = spec
            .effort
            .clone()
            .map(|effort| Deferred::new(first_prompt, effort, now_ms));
        let brain = Self {
            id,
            harness: session.harness(),
            model: session.spec().model.clone(),
            phase: BrainPhase::Thinking {
                ask,
                since_ms: now_ms,
            },
            turns: 1,
            retried: false,
            handled: Vec::new(),
            deferred,
            cost_usd: 0.0,
            session_snapshot: None,
            session: Some(session),
        };
        Ok((brain, exec))
    }

    /// Picks the session up again after a restart. Run [`Self::status_exec`] next.
    pub fn reattach(&mut self, spec: SessionSpec) -> Result<(), SessionError> {
        let mut session = match self.session_snapshot.as_deref() {
            Some(snap) => {
                Session::restore(snap, spec.clone()).or_else(|_| Session::attach(spec, CLIENT_TAG))?
            }
            None => Session::attach(spec, CLIENT_TAG)?,
        };
        session.set_policy(Some(Policy {
            rules: crate::policy::brain_rules(),
        }));
        self.session = Some(session);
        Ok(())
    }

    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    /// The acc session's spec, once it is launched or reattached.
    pub fn spec(&self) -> Option<&SessionSpec> {
        self.session.as_ref().map(Session::spec)
    }

    pub fn is_live(&self) -> bool {
        !matches!(self.phase, BrainPhase::Gone)
    }

    pub fn thinking_about(&self) -> Option<&AskId> {
        match &self.phase {
            BrainPhase::Thinking { ask, .. } => Some(ask),
            _ => None,
        }
    }

    pub fn snapshot(&self) -> Option<String> {
        self.session.as_ref().and_then(Session::snapshot)
    }

    pub fn offset(&self) -> u64 {
        self.session.as_ref().map_or(0, Session::offset)
    }

    pub fn is_replaying(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_replaying)
    }

    pub fn is_alive(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_alive)
    }

    pub fn state(&self) -> Option<RunState> {
        self.session
            .as_ref()
            .and_then(|s| s.state())
            .map(|s| s.state.clone())
    }

    // ---- input from the host -------------------------------------------------

    pub fn on_launched(&mut self, stdout: &str) -> Vec<Update> {
        self.session
            .as_mut()
            .map(|s| s.on_launched(stdout))
            .unwrap_or_default()
    }

    pub fn on_status(&mut self, stdout: &str, now_ms: u64) -> Vec<Update> {
        self.session
            .as_mut()
            .map(|s| s.on_status(stdout, now_ms))
            .unwrap_or_default()
    }

    pub fn feed(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Update> {
        self.session
            .as_mut()
            .map(|s| s.feed(bytes, now_ms))
            .unwrap_or_default()
    }

    pub fn on_exit(&mut self, code: Option<i32>, now_ms: u64) -> Vec<Update> {
        let text = code.map(|c| c.to_string()).unwrap_or_default();
        self.session
            .as_mut()
            .map(|s| s.on_exit_file(&text, now_ms))
            .unwrap_or_default()
    }

    /// What `updates` mean. Replayed history is only counted, never acted on.
    pub fn observe(&mut self, updates: &[Update]) -> Vec<BrainObs> {
        let mut out = Vec::new();
        for u in updates {
            match &u.event {
                AccEvent::Usage(_) => {
                    if let Some(c) = self.session.as_ref().and_then(|s| s.usage().cost_usd) {
                        self.cost_usd = c;
                    }
                }
                _ if u.replay => {}
                AccEvent::StateChanged(st) => {
                    if let RunState::Blocked {
                        kind: BlockKind::Auth,
                    } = st.state
                    {
                        out.push(BrainObs::Auth(st.message.clone().unwrap_or_default()));
                    }
                }
                AccEvent::TurnCompleted => out.push(self.turn_end()),
                AccEvent::Exited { code } => out.push(BrainObs::Exited(*code)),
                _ => {}
            }
        }
        out
    }

    /// How the turn that just completed ended.
    pub fn turn_end(&self) -> BrainObs {
        match self.state() {
            Some(RunState::Error) => BrainObs::TurnError(
                self.session
                    .as_ref()
                    .and_then(|s| s.state())
                    .and_then(|s| s.message.clone())
                    .unwrap_or_else(|| "the turn failed".to_owned()),
            ),
            _ => BrainObs::TurnDone,
        }
    }

    /// Approvals the policy didn't settle are rejected, questions are told
    /// to decide; the execs answer them. A login is returned as an error.
    pub fn answer_requests(&mut self) -> Result<Vec<Exec>, String> {
        let open: Vec<Permission> = match &self.session {
            Some(s) if !s.is_replaying() => s
                .permissions()
                .iter()
                .filter(|p| !self.handled.contains(&p.request_id))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        let mut execs = Vec::new();
        for p in open {
            self.handled.push(p.request_id.clone());
            let option = match p.kind {
                BlockKind::Permission => match acc_core::policy::option_for(PolicyDecision::Deny, &p.options)
                {
                    Some(o) => o.id.clone(),
                    None => continue,
                },
                BlockKind::Question => {
                    let n = acc_core::question::parse(&p.native).len().max(1);
                    acc_core::question::encode(&vec![vec![DECIDE_YOURSELF.to_owned()]; n])
                }
                BlockKind::Auth => {
                    return Err(format!("the orchestrator's harness needs a login: {}", p.title));
                }
            };
            let cmd = Command::AnswerPermission {
                request_id: p.request_id.clone(),
                option_id: option,
            };
            if let Some(exec) = self.command_exec(cmd) {
                execs.push(exec);
            }
        }
        Ok(execs)
    }

    /// The brain's last message.
    pub fn last_message(&self) -> Option<String> {
        self.session.as_ref().and_then(|s| {
            s.transcript()
                .top_level()
                .filter(|i| i.kind == ItemKind::AgentMessage && !i.text.trim().is_empty())
                .last()
                .map(|i| i.text.clone())
        })
    }

    // ---- output to the harness -----------------------------------------------

    /// Sends a turn.
    pub fn prompt_exec(&mut self, text: String) -> Option<Exec> {
        self.command_exec(Command::prompt(text))
    }

    /// Runs `cmd` on the session, as an input exec when it writes anything.
    pub fn command_exec(&mut self, cmd: Command) -> Option<Exec> {
        let s = self.session.as_mut()?;
        match s.command(cmd).ok()? {
            Outbound::Send(bytes) if !bytes.is_empty() => Some(s.input_exec(bytes)),
            _ => None,
        }
    }

    /// The commands a held-back first prompt is ready to go out with.
    pub fn deferred_due(&self, now_ms: u64) -> Option<Vec<Command>> {
        let d = self.deferred.as_ref()?;
        d.due(self.session.as_ref()?, now_ms)
    }

    /// What the session wants written now: bytes for the harness, and a
    /// rewritten `spec.json`.
    pub fn flush(&mut self) -> (Option<Exec>, Option<Exec>) {
        let Some(s) = &mut self.session else {
            return (None, None);
        };
        let bytes = s.take_transmit();
        let input = (!bytes.is_empty()).then(|| s.input_exec(bytes));
        (input, s.take_spec_write())
    }

    pub fn status_exec(&self) -> Option<Exec> {
        self.session.as_ref().map(Session::status_exec)
    }

    pub fn kill_exec(&self) -> Option<Exec> {
        self.session.as_ref().map(Session::kill_exec)
    }

    pub fn remove_exec(&self) -> Option<Exec> {
        self.session.as_ref().map(Session::remove_exec)
    }
}
