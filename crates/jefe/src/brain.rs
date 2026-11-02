//! The harness brain: each consultation is a one-shot acc session on the
//! user's own harness, in the job's repository, under a read-only policy.
//! It may look around; it can't change anything, and its only output that
//! counts is one `jefe-decision` block, which the loop still validates.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use acc_core::Harness;
use acc_core::command::Command;
use acc_core::event::ItemKind;
use acc_core::harness::Launcher;
use acc_core::policy::{Decision as PolicyDecision, Policy};
use acc_core::sandbox::{Preset, Sandbox};
use acc_core::session::{LaunchRequest, Outbound, Session};
use acc_core::{BlockKind, Event as AccEvent, Exec};
use acc_ssh::{Detached, Host, Read};
use jefe_core::brain::wire;
use jefe_core::prompts::Prompts;
use jefe_core::{AskId, Decision, DecisionSource, JobId, Situation};
use serde::{Deserialize, Serialize};

const ASK_FOR_BLOCK: &str = "Your reply had no valid ```jefe-decision block. Reply again with a short rationale \
     and exactly one ```jefe-decision block holding one of the allowed actions.";
const DECIDE_YOURSELF: &str = "Nobody can answer questions here: decide yourself and say what you assumed.";
/// How long to wait for the harness to exit after it was told to.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// What a harness brain needs besides the situation. Saved with the job as
/// `brain.json` so `attach` brings the same brain back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessBrain {
    pub job: JobId,
    /// Where the brain runs: the job's repository on its host.
    pub repo: String,
    pub harness: Harness,
    pub model: Option<String>,
    pub permission: Option<String>,
    pub launcher: Option<Launcher>,
    /// Run it in acc's read-only sandbox.
    pub sandbox: bool,
    pub timeout_ms: u64,
    pub prompts: Prompts,
}

/// Brain session numbers, unique within this process. A session left by a
/// process that died is stopped and removed before its id is reused.
static NEXT: AtomicU32 = AtomicU32::new(1);

/// One consultation's transcript, for `brain/<ask>.md`.
#[derive(Debug, Default)]
pub struct Exchange {
    pub session: String,
    pub prompt: String,
    pub replies: Vec<String>,
    pub outcome: String,
}

impl Exchange {
    pub fn markdown(&self) -> String {
        let mut s = format!("# {}\n\n## Prompt\n\n{}\n", self.session, self.prompt.trim_end());
        for (i, r) in self.replies.iter().enumerate() {
            s.push_str(&format!("\n## Reply {}\n\n{}\n", i + 1, r.trim_end()));
        }
        s.push_str(&format!("\n## Outcome\n\n{}\n", self.outcome));
        s
    }
}

impl HarnessBrain {
    fn source(&self) -> DecisionSource {
        DecisionSource::Brain {
            harness: Some(self.harness),
            model: self.model.clone(),
        }
    }

    /// Asks the brain about `situation`. `Err` means the rules must decide.
    /// The exchange is written to `log_dir/<ask>.md` either way.
    pub async fn consult(
        &self,
        host: Arc<Host>,
        ask: &AskId,
        situation: &Situation,
        log_dir: Option<PathBuf>,
    ) -> Result<Decision, String> {
        let id = jefe_core::ids::brain_id(&self.job, NEXT.fetch_add(1, Ordering::Relaxed));
        let mut ex = Exchange {
            session: id.clone(),
            prompt: wire::render_prompt(situation, &self.prompts),
            ..Exchange::default()
        };
        let timeout = Duration::from_millis(self.timeout_ms.max(1));
        let result = match tokio::time::timeout(timeout, self.run(&host, &id, &mut ex)).await {
            Ok(r) => r,
            Err(_) => Err(format!("no decision within {} s", timeout.as_secs())),
        };
        clean_up(&host, &id).await;
        ex.outcome = match &result {
            Ok(d) => serde_json::to_string(d).unwrap_or_default(),
            Err(why) => format!("failed: {why}"),
        };
        if let Some(dir) = log_dir {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(format!("{ask}.md")), ex.markdown());
        }
        result
    }

    async fn run(&self, host: &Arc<Host>, id: &str, ex: &mut Exchange) -> Result<Decision, String> {
        clean_up(host, id).await;
        let req = LaunchRequest {
            harness: Some(self.harness),
            cwd: self.repo.clone(),
            model: self.model.clone(),
            permission: self.permission.clone(),
            title: Some(format!("jefe brain for {}", self.job)),
            prompt: Some(ex.prompt.clone()),
            labels: BTreeMap::from([("jefe_job".to_owned(), self.job.0.clone())]),
            launcher: self.launcher.clone(),
            sandbox: self.sandbox.then(|| Sandbox {
                preset: Preset::ReadOnly,
                ..Sandbox::default()
            }),
            ..LaunchRequest::default()
        };
        let now = crate::driver::now_ms();
        let (mut session, exec) =
            Session::launch(req, id, jefe_core::worker::CLIENT_TAG, now).map_err(|e| e.to_string())?;
        session.set_policy(Some(Policy {
            rules: jefe_core::policy::brain_rules(),
        }));
        let out = run_exec(host, &exec).await?;
        session.on_launched(&out);
        flush(host, &mut session).await?;

        let log = Detached::attach(host.clone(), id, 0).map_err(|e| e.to_string())?;
        let mut handled: Vec<String> = Vec::new();
        let mut retried = false;
        loop {
            let bytes = match log.read().await.map_err(|e| e.to_string())? {
                Read::Data(b) => b,
                Read::Exited(code) => {
                    return Err(format!(
                        "the brain's harness exited ({}) before deciding",
                        code.map_or("no code".to_owned(), |c| c.to_string())
                    ));
                }
            };
            let updates = session.feed(&bytes, crate::driver::now_ms());
            answer_requests(host, &mut session, &mut handled).await?;
            flush(host, &mut session).await?;
            if !updates.iter().any(|u| u.event == AccEvent::TurnCompleted) {
                continue;
            }
            let reply = last_message(&session).unwrap_or_default();
            ex.replies.push(reply.clone());
            if let Some(d) = wire::parse_decision(&reply, self.source()) {
                return Ok(d);
            }
            if retried {
                return Err("no valid jefe-decision block, twice".to_owned());
            }
            retried = true;
            let cmd = Command::Prompt {
                text: ASK_FOR_BLOCK.to_owned(),
                attachments: Vec::new(),
            };
            match session.command(cmd).map_err(|e| e.to_string())? {
                Outbound::Send(bytes) => {
                    run_exec(host, &session.input_exec(bytes)).await?;
                }
                Outbound::Relaunch(_) => return Err("the brain's harness can't take another turn".to_owned()),
            }
        }
    }
}

/// Approvals no rule allowed are rejected; questions are told to decide.
async fn answer_requests(
    host: &Arc<Host>,
    session: &mut Session,
    handled: &mut Vec<String>,
) -> Result<(), String> {
    let open: Vec<_> = session
        .permissions()
        .iter()
        .filter(|p| !handled.contains(&p.request_id))
        .cloned()
        .collect();
    for p in open {
        handled.push(p.request_id.clone());
        let option = match p.kind {
            BlockKind::Permission => match acc_core::policy::option_for(PolicyDecision::Deny, &p.options) {
                Some(o) => o.id.clone(),
                None => continue,
            },
            BlockKind::Question => {
                let n = acc_core::question::parse(&p.native).len().max(1);
                acc_core::question::encode(&vec![vec![DECIDE_YOURSELF.to_owned()]; n])
            }
            BlockKind::Auth => return Err(format!("the brain's harness needs a login: {}", p.title)),
        };
        let cmd = Command::AnswerPermission {
            request_id: p.request_id.clone(),
            option_id: option,
        };
        if let Ok(Outbound::Send(bytes)) = session.command(cmd)
            && !bytes.is_empty()
        {
            run_exec(host, &session.input_exec(bytes)).await?;
        }
    }
    Ok(())
}

/// Writes what the session queued: bytes for the harness, its spec file.
async fn flush(host: &Arc<Host>, session: &mut Session) -> Result<(), String> {
    let bytes = session.take_transmit();
    if !bytes.is_empty() {
        run_exec(host, &session.input_exec(bytes)).await?;
    }
    if let Some(exec) = session.take_spec_write() {
        run_exec(host, &exec).await?;
    }
    Ok(())
}

async fn run_exec(host: &Host, exec: &Exec) -> Result<String, String> {
    let out = host
        .exec(&exec.script, &exec.stdin)
        .await
        .map_err(|e| e.to_string())?
        .ok()
        .map_err(|e| e.to_string())?;
    Ok(out.stdout_str())
}

fn last_message(session: &Session) -> Option<String> {
    session
        .transcript()
        .top_level()
        .filter(|i| i.kind == ItemKind::AgentMessage && !i.text.trim().is_empty())
        .last()
        .map(|i| i.text.clone())
}

/// Stops the session if it runs and deletes its files.
async fn clean_up(host: &Arc<Host>, id: &str) {
    let Ok(d) = Detached::attach(host.clone(), id, 0) else {
        return;
    };
    if matches!(d.status().await, Ok(acc_ssh::Status::Missing) | Err(_)) {
        return;
    }
    if d.stop().await.is_ok() {
        let deadline = tokio::time::Instant::now() + EXIT_GRACE;
        while tokio::time::Instant::now() < deadline {
            if !matches!(d.status().await, Ok(acc_ssh::Status::Alive { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let _ = d.remove().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_reads_in_order() {
        let ex = Exchange {
            session: "jefe-j-b1".to_owned(),
            prompt: "decide".to_owned(),
            replies: vec!["no block".to_owned(), "block".to_owned()],
            outcome: "failed: x".to_owned(),
        };
        let md = ex.markdown();
        let (p, r1, r2, o) = (
            md.find("## Prompt"),
            md.find("## Reply 1"),
            md.find("## Reply 2"),
            md.find("## Outcome"),
        );
        assert!(p < r1 && r1 < r2 && r2 < o, "{md}");
    }
}
