//! A worker: one acc session in one worktree. This is the only module that
//! drives `acc_core::session::Session`, so an acc bump stays local.

use acc_core::command::Command;
use acc_core::event::{ItemKind, Plan, Quota, Usage};
use acc_core::policy::{Decision as PolicyDecision, Policy, Rule};
use acc_core::session::{LaunchRequest, Outbound, Permission, Session, SessionError, SessionSpec, Update};
use acc_core::{BlockKind, Event as AccEvent, Exec, Harness, RunState};
use serde::{Deserialize, Serialize};

use crate::ids::{HostId, TaskId, WorkerId};

/// How jefe names itself to acc (request ids, spec files).
pub const CLIENT_TAG: &str = "jefe";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum WorkerPhase {
    /// Its worktree is being made.
    Preparing,
    /// The harness is starting.
    Launching,
    Working,
    /// The turn ended, but a background subagent will start another.
    Waiting,
    Blocked {
        kind: BlockKind,
        request_id: String,
    },
    /// It reported; it idles until prompted, merged or discarded.
    Reported,
    Stalled {
        since_ms: u64,
    },
    Errored {
        message: String,
    },
    Exited {
        code: Option<i32>,
    },
    /// Its worktree and branch are being deleted.
    Discarding,
    /// Stopped for good (its branch may be kept).
    Gone,
}

impl WorkerPhase {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Launching => "launching",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Blocked { .. } => "blocked",
            Self::Reported => "reported",
            Self::Stalled { .. } => "stalled",
            Self::Errored { .. } => "errored",
            Self::Exited { .. } => "exited",
            Self::Discarding => "discarding",
            Self::Gone => "gone",
        }
    }
}

/// What a batch of acc updates meant for jefe.
#[derive(Debug, Clone, PartialEq)]
pub enum Obs {
    /// The agent did something.
    Progress,
    TurnDone,
    /// The turn ended but a background subagent will wake the agent again.
    TurnWaiting,
    TurnError(String),
    Auth(String),
    Quota(Quota),
    Exited(Option<i32>),
    Notice(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Worker {
    pub id: WorkerId,
    pub task: Option<TaskId>,
    pub harness: Harness,
    pub host: HostId,
    /// The acc worktree it works in; a replacement keeps its predecessor's.
    pub worktree: String,
    pub branch: String,
    /// Its directory on the host, once the worktree exists.
    pub cwd: Option<String>,
    pub phase: WorkerPhase,
    /// Prompts after the first.
    pub turns: u32,
    /// Replacements before this one, for the same task.
    pub respawns: u32,
    pub stalls: u32,
    /// Times it was asked for its missing result block.
    pub result_nudges: u32,
    pub last_progress_ms: u64,
    pub plan: Option<Plan>,
    pub usage: Usage,
    pub quota: Option<Quota>,
    /// Set when another worker took over its task.
    pub replaced_by: Option<WorkerId>,
    /// Its worktree and branch were deleted.
    pub discarded: bool,
    /// Requests it raised that jefe already dealt with.
    pub handled: Vec<String>,
    /// The launch to make once its predecessor has stopped. Kept as a
    /// request, not an exec: the session is only built when it launches, so
    /// a restart in between loses nothing.
    pub pending_launch: Option<LaunchRequest>,
    /// Writes that were running when jefe stopped, sent again at
    /// `resend_at` unless the worker shows signs of life first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsent: Vec<Exec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resend_at: Option<u64>,
    /// [`Session::snapshot`] as of the job's last snapshot.
    pub session_snapshot: Option<String>,
    #[serde(skip)]
    session: Option<Session>,
}

impl Worker {
    pub fn new(
        id: WorkerId,
        task: TaskId,
        harness: Harness,
        host: HostId,
        worktree: String,
        now_ms: u64,
    ) -> Self {
        let branch = acc_core::worktree::branch(&worktree);
        Self {
            id,
            task: Some(task),
            harness,
            host,
            worktree,
            branch,
            cwd: None,
            phase: WorkerPhase::Preparing,
            turns: 0,
            respawns: 0,
            stalls: 0,
            result_nudges: 0,
            last_progress_ms: now_ms,
            plan: None,
            usage: Usage::default(),
            quota: None,
            replaced_by: None,
            discarded: false,
            handled: Vec::new(),
            pending_launch: None,
            unsent: Vec::new(),
            resend_at: None,
            session_snapshot: None,
            session: None,
        }
    }

    /// Holds a slot: started and not finished with.
    pub fn is_live(&self) -> bool {
        self.replaced_by.is_none() && !matches!(self.phase, WorkerPhase::Gone | WorkerPhase::Discarding)
    }

    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Starts its session. Run the returned exec on the host.
    pub fn launch(
        &mut self,
        req: LaunchRequest,
        rules: Vec<Rule>,
        now_ms: u64,
    ) -> Result<Exec, SessionError> {
        let (mut session, exec) = Session::launch(req, self.id.as_str(), CLIENT_TAG, now_ms)?;
        // jefe holds the session's policy; nobody else answers for it.
        session.set_policy(Some(Policy { rules }));
        self.harness = session.harness();
        self.session = Some(session);
        Ok(exec)
    }

    /// Whether `req` would launch, without keeping anything.
    pub fn check_launch(req: &LaunchRequest, now_ms: u64) -> Result<(), SessionError> {
        Session::launch(req.clone(), "jefe-check", CLIENT_TAG, now_ms).map(drop)
    }

    /// Picks its session up again after a restart, from the snapshot if it
    /// still fits, else by attaching afresh. Run [`Worker::status_exec`] next.
    pub fn reattach(&mut self, spec: SessionSpec, rules: Vec<Rule>) -> Result<(), SessionError> {
        let mut session = match self.session_snapshot.as_deref() {
            Some(snap) => {
                Session::restore(snap, spec.clone()).or_else(|_| Session::attach(spec, CLIENT_TAG))?
            }
            None => Session::attach(spec, CLIENT_TAG)?,
        };
        session.set_policy(Some(Policy { rules }));
        self.session = Some(session);
        Ok(())
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

    pub fn native_id(&self) -> Option<String> {
        self.session.as_ref().and_then(|s| s.spec().native_id.clone())
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

    /// What `updates` mean, keeping plan, usage and quota up to date.
    /// History replayed on attach is only counted, never acted on.
    pub fn observe(&mut self, updates: &[Update]) -> Vec<Obs> {
        let mut out = Vec::new();
        for u in updates {
            match &u.event {
                AccEvent::Usage(_) => {
                    if let Some(s) = &self.session {
                        self.usage = s.usage().clone();
                    }
                }
                AccEvent::Plan(p) => self.plan = Some(p.clone()),
                AccEvent::Quota(q) => {
                    self.quota = Some(q.clone());
                    if !u.replay {
                        out.push(Obs::Quota(q.clone()));
                    }
                }
                _ if u.replay => {}
                AccEvent::ItemStarted { .. }
                | AccEvent::ItemDelta { .. }
                | AccEvent::ItemUpdated { .. }
                | AccEvent::ItemCompleted { .. } => {
                    if out.last() != Some(&Obs::Progress) {
                        out.push(Obs::Progress);
                    }
                }
                AccEvent::StateChanged(st) => {
                    if let RunState::Blocked {
                        kind: BlockKind::Auth,
                    } = st.state
                    {
                        out.push(Obs::Auth(st.message.clone().unwrap_or_default()));
                    }
                }
                AccEvent::TurnCompleted => out.push(self.turn_end()),
                AccEvent::Exited { code } => out.push(Obs::Exited(*code)),
                AccEvent::Notice { message } => out.push(Obs::Notice(message.clone())),
                _ => {}
            }
        }
        out
    }

    /// How the turn that just completed ended.
    pub fn turn_end(&self) -> Obs {
        match self.state() {
            Some(RunState::Waiting) => Obs::TurnWaiting,
            Some(RunState::Error) => Obs::TurnError(
                self.session
                    .as_ref()
                    .and_then(|s| s.state())
                    .and_then(|s| s.message.clone())
                    .unwrap_or_else(|| "the turn failed".to_owned()),
            ),
            _ => Obs::TurnDone,
        }
    }

    /// Approvals and questions waiting that jefe hasn't dealt with yet.
    pub fn open_requests(&self) -> Vec<Permission> {
        let Some(s) = &self.session else { return Vec::new() };
        if s.is_replaying() {
            return Vec::new();
        }
        s.permissions()
            .iter()
            .filter(|p| !self.handled.contains(&p.request_id))
            .cloned()
            .collect()
    }

    /// The agent's last message.
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

    /// Turns a command into an input exec, or a relaunch request.
    pub fn command(&mut self, cmd: Command) -> Result<Outbound, SessionError> {
        match &mut self.session {
            Some(s) => s.command(cmd),
            None => Err(SessionError::NoHarness),
        }
    }

    /// Answers a permission request with the reject option, if it has one.
    pub fn reject(&mut self, p: &Permission) -> Option<Exec> {
        let option = acc_core::policy::option_for(PolicyDecision::Deny, &p.options)?
            .id
            .clone();
        self.answer(&p.request_id, option)
    }

    /// Answers a request with `option_id` (or encoded question answers).
    pub fn answer(&mut self, request_id: &str, option_id: String) -> Option<Exec> {
        let cmd = Command::AnswerPermission {
            request_id: request_id.to_owned(),
            option_id,
        };
        match self.command(cmd).ok()? {
            Outbound::Send(bytes) => self.input_exec(bytes),
            Outbound::Relaunch(_) => None,
        }
    }

    pub fn input_exec(&self, bytes: Vec<u8>) -> Option<Exec> {
        if bytes.is_empty() {
            return None;
        }
        self.session.as_ref().map(|s| s.input_exec(bytes))
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

    pub fn diff_exec(&self) -> Option<Exec> {
        self.session.as_ref().and_then(|s| s.worktree_diff_exec().ok())
    }

    pub fn merge_exec(&self, message: &str, squash: bool) -> Option<Exec> {
        self.session
            .as_ref()
            .and_then(|s| s.worktree_merge_exec(message, squash).ok())
    }

    pub fn discard_exec(&self) -> Option<Exec> {
        self.session.as_ref().and_then(|s| s.worktree_discard_exec().ok())
    }

    /// A launch request that starts this worker's harness again in the same
    /// worktree, resuming its conversation when the harness said its id.
    pub fn respawn_request(&self, prompt: String) -> Option<LaunchRequest> {
        let spec = self.session.as_ref()?.spec();
        Some(LaunchRequest {
            harness: Some(spec.harness),
            cwd: spec.cwd.clone(),
            model: spec.model.clone(),
            permission: spec.permission.clone(),
            resume: spec.native_id.clone(),
            title: spec.title.clone(),
            prompt: Some(prompt),
            parent: Some(spec.id.clone()),
            replaces_parent: true,
            labels: spec.labels.clone(),
            launcher: spec.launcher.clone(),
            worktree: spec.worktree.clone(),
            sandbox: spec.sandbox.clone(),
            ..LaunchRequest::default()
        })
    }
}
