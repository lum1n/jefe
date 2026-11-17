//! UniFFI surface of jefe-core for Swift (iOS/macOS) and Kotlin.
//!
//! Sans-IO, like jefe-core: a [`JefeJob`] turns inputs into effects and the
//! app runs them over whatever transport it has (its SSH channels, a local
//! shell), then feeds the results back. Everything crosses the boundary as
//! JSON in jefe's own shapes (`docs/PROTOCOL.md`, `docs/EMBED.md`), except
//! an exec's stdin, which travels as bytes beside the effect.
//!
//! Objects can be shared across threads. Panics are caught by UniFFI and
//! surfaced as errors instead of crashing the app.

use std::sync::{Arc, Mutex, MutexGuard};

use acc_core::session::SessionSpec;
use jefe_core::brain::{Brain as _, RulesBrain};
use jefe_core::io::Effect;
use jefe_core::{Hosted, Input, JobSpec, TeamPreset};

uniffi::setup_scaffolding!();

/// Version of jefe-core.
#[uniffi::export]
pub fn jefe_version() -> String {
    jefe_core::VERSION.to_owned()
}

/// The protocol major version the JSON follows.
#[uniffi::export]
pub fn jefe_protocol_version() -> u32 {
    jefe_core::PROTOCOL_VERSION
}

/// Errors from the bindings.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum JefeError {
    /// JSON that isn't the shape asked for.
    // Fields are not called `message`: Kotlin errors extend Throwable.
    #[error("invalid JSON: {reason}")]
    InvalidJson { reason: String },
    /// A snapshot that can't be restored (another version, unreadable).
    #[error("can't restore the job: {reason}")]
    Restore { reason: String },
    /// A spec or preset that can't run.
    #[error("{reason}")]
    Invalid { reason: String },
    /// The job's lock was poisoned by a panic in an earlier call.
    #[error("the job is unusable after an earlier failure")]
    Poisoned,
}

fn bad_json(e: serde_json::Error) -> JefeError {
    JefeError::InvalidJson {
        reason: e.to_string(),
    }
}

/// A script to run on the job's host with `stdin` fed to it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JefeExec {
    pub script: String,
    pub stdin: Vec<u8>,
}

impl From<acc_core::Exec> for JefeExec {
    fn from(e: acc_core::Exec) -> Self {
        Self {
            script: e.script,
            stdin: e.stdin,
        }
    }
}

/// Answers the loop's `Consult` effects with the rules (the brain when
/// `kind = rules`, the fallback otherwise), so an app never sees them.
fn settle_consults(hosted: &mut Hosted, mut effects: Vec<Effect>, now_ms: u64) -> Vec<Effect> {
    let mut out = Vec::new();
    loop {
        let mut pending = Vec::new();
        for e in effects {
            match e {
                Effect::Consult { ask, situation } => pending.push((ask, RulesBrain.decide(&situation))),
                other => out.push(other),
            }
        }
        if pending.is_empty() {
            return out;
        }
        effects = Vec::new();
        for (ask, decision) in pending {
            effects.extend(hosted.step(Input::Decided { ask, decision }, now_ms));
        }
    }
}

/// One effect of the loop: `json` is the `Effect` as jefe serializes it
/// (`{"effect":"run",...}`), with a run's `exec.stdin` emptied there and
/// given as `stdin` instead, so the app decodes small JSON and moves bytes.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JefeEffect {
    pub json: String,
    pub stdin: Vec<u8>,
}

fn encode(effects: Vec<Effect>) -> Vec<JefeEffect> {
    effects
        .into_iter()
        .map(|e| match e {
            Effect::Run {
                id,
                host,
                mut exec,
                purpose,
            } => {
                let stdin = std::mem::take(&mut exec.stdin);
                let json = serde_json::to_string(&Effect::Run {
                    id,
                    host,
                    exec,
                    purpose,
                })
                .unwrap_or_default();
                JefeEffect { json, stdin }
            }
            other => JefeEffect {
                json: serde_json::to_string(&other).unwrap_or_default(),
                stdin: Vec::new(),
            },
        })
        .collect()
}

/// One orchestrator job, driven by the app. See `docs/EMBED.md`:
/// 1. `start` (or take the lease with `lease_exec`, read the snapshot with
///    `load_job_exec` and `open`), then run every effect `take_effects` gives.
/// 2. For each result, `step` with the `Input` as JSON (`exec_done`,
///    `bytes`, `log_ended`, `tick`, `human`), and run what comes back.
/// 3. Send a `tick` at `next_deadline`. Saves and lease renewals are among
///    the effects; nothing else is needed for state on the host.
#[derive(Debug, uniffi::Object)]
pub struct JefeJob {
    inner: Mutex<Hosted>,
    pending: Mutex<Vec<JefeEffect>>,
}

impl JefeJob {
    fn lock(&self) -> Result<MutexGuard<'_, Hosted>, JefeError> {
        self.inner.lock().map_err(|_| JefeError::Poisoned)
    }
}

#[uniffi::export]
impl JefeJob {
    /// Starts a job from a `JobSpec` as JSON (see `new_job_spec_json` and
    /// `apply_preset`), keeping its state on its host under `client`'s lease.
    #[uniffi::constructor]
    pub fn start(spec_json: String, client: String, now_ms: u64) -> Result<Arc<Self>, JefeError> {
        let spec: JobSpec = serde_json::from_str(&spec_json).map_err(bad_json)?;
        if !jefe_core::ids::valid_job_id(spec.id.as_str()) {
            return Err(JefeError::Invalid {
                reason: format!("bad job id {:?}: up to 40 letters, digits, '-' or '_'", spec.id),
            });
        }
        let (mut hosted, effects) = Hosted::start(spec, &client, now_ms);
        let effects = settle_consults(&mut hosted, effects, now_ms);
        Ok(Arc::new(Self {
            inner: Mutex::new(hosted),
            pending: Mutex::new(encode(effects)),
        }))
    }

    /// Picks a job up from the snapshot `load_job_exec` brought back, once
    /// `lease_exec` said `client` holds the lease. `session_specs_json` are
    /// the `spec.json` lines of the acc sessions the host lists now (acc's
    /// `list_script` / `parse_list`).
    #[uniffi::constructor]
    pub fn open(
        snapshot: String,
        session_specs_json: Vec<String>,
        client: String,
        now_ms: u64,
    ) -> Result<Arc<Self>, JefeError> {
        let specs: Vec<SessionSpec> = session_specs_json
            .iter()
            .filter_map(|j| SessionSpec::from_json(j))
            .collect();
        let (mut hosted, effects) =
            Hosted::open(&snapshot, &specs, &client, now_ms).map_err(|e| JefeError::Restore {
                reason: e.to_string(),
            })?;
        let effects = settle_consults(&mut hosted, effects, now_ms);
        Ok(Arc::new(Self {
            inner: Mutex::new(hosted),
            pending: Mutex::new(encode(effects)),
        }))
    }

    /// The effects of `start` or `open`, once.
    pub fn take_effects(&self) -> Vec<JefeEffect> {
        self.pending
            .lock()
            .map(|mut p| std::mem::take(&mut *p))
            .unwrap_or_default()
    }

    /// Advances the loop with one `Input` as JSON; run what comes back.
    pub fn step(&self, input_json: String, now_ms: u64) -> Result<Vec<JefeEffect>, JefeError> {
        let input: Input = serde_json::from_str(&input_json).map_err(bad_json)?;
        let mut h = self.lock()?;
        let effects = h.step(input, now_ms);
        Ok(encode(settle_consults(&mut h, effects, now_ms)))
    }

    /// Gives the lease up on purpose (the app goes to the background):
    /// workers keep going, decisions wait for the next client.
    pub fn release(&self, now_ms: u64) -> Result<Vec<JefeEffect>, JefeError> {
        let mut h = self.lock()?;
        Ok(encode(h.release(now_ms)))
    }

    /// When the loop next needs a `tick`, in ms since the epoch.
    pub fn next_deadline(&self) -> Result<Option<u64>, JefeError> {
        Ok(self.lock()?.next_deadline())
    }

    /// The job as JSON (`snapshot.job` of the protocol), for display.
    pub fn job_json(&self) -> Result<String, JefeError> {
        let h = self.lock()?;
        serde_json::to_string(h.jefe().job()).map_err(bad_json)
    }

    /// The whole snapshot, for the app's own cache.
    pub fn snapshot(&self) -> Result<String, JefeError> {
        Ok(self.lock()?.snapshot())
    }

    /// The job's status name (`planning`, `running`, `done`, ...).
    pub fn status(&self) -> Result<String, JefeError> {
        Ok(self.lock()?.jefe().job().status.name().to_owned())
    }

    /// The job is over and its last save and release were issued.
    pub fn is_finished(&self) -> Result<bool, JefeError> {
        Ok(self.lock()?.is_finished())
    }

    /// Another client took the lease; this one stopped driving.
    pub fn lease_lost(&self) -> Result<bool, JefeError> {
        Ok(self.lock()?.lease_lost())
    }

    /// The orchestrator's acc session id, when it runs as one (open it
    /// read-only as an agent view).
    pub fn brain_session_id(&self) -> Result<Option<String>, JefeError> {
        Ok(self.lock()?.jefe().brain_session().map(|b| b.id.clone()))
    }
}

// ---- the host-side store --------------------------------------------------

/// Takes or renews job `id`'s lease for `client`; parse the stdout with
/// `parse_lease`. `ttl_ms` 0 takes the default (30 s).
#[uniffi::export]
pub fn lease_exec(job_id: String, client: String, ttl_ms: u64) -> JefeExec {
    let ttl = if ttl_ms == 0 {
        jefe_core::store::LEASE_TTL_MS
    } else {
        ttl_ms
    };
    jefe_core::store::lease_exec(&jefe_core::JobId(job_id), &client, ttl).into()
}

/// Who holds a lease after `lease_exec`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JefeLease {
    /// The caller holds it now.
    pub ok: bool,
    pub holder: String,
    pub at_secs: u64,
    pub ttl_secs: u64,
}

#[uniffi::export]
pub fn parse_lease(stdout: String) -> Option<JefeLease> {
    jefe_core::store::parse_lease(&stdout).map(|l| JefeLease {
        ok: l.ok,
        holder: l.holder,
        at_secs: l.at_secs,
        ttl_secs: l.ttl_secs,
    })
}

/// Gives job `id`'s lease up, if `client` holds it.
#[uniffi::export]
pub fn release_exec(job_id: String, client: String) -> JefeExec {
    jefe_core::store::release_exec(&jefe_core::JobId(job_id), &client).into()
}

/// Lists the jobs on a host; parse the stdout with `parse_job_list`.
#[uniffi::export]
pub fn list_jobs_exec() -> JefeExec {
    jefe_core::store::list_exec().into()
}

/// A job on a host.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JefeJobListing {
    pub id: String,
    pub status: String,
    pub updated_secs: u64,
    /// The client driving it now, if any.
    pub holder: Option<String>,
    pub goal: String,
}

#[uniffi::export]
pub fn parse_job_list(stdout: String) -> Vec<JefeJobListing> {
    jefe_core::store::parse_list(&stdout)
        .into_iter()
        .map(|j| JefeJobListing {
            id: j.id.0,
            status: j.status,
            updated_secs: j.updated_secs,
            holder: j.holder,
            goal: j.goal,
        })
        .collect()
}

/// Reads job `id`'s snapshot and its last `tail` events (0: the default);
/// parse the stdout with `parse_job_load`. Exit 6 when there is no such job.
#[uniffi::export]
pub fn load_job_exec(job_id: String, tail: u32) -> JefeExec {
    let tail = if tail == 0 {
        jefe_core::store::LOAD_TAIL
    } else {
        tail as usize
    };
    jefe_core::store::load_exec(&jefe_core::JobId(job_id), tail).into()
}

/// What `load_job_exec` brought back: the snapshot for `JefeJob::open`,
/// and the last events as JSON lines (`Stamped`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct JefeLoaded {
    pub snapshot: String,
    pub events_json: Vec<String>,
}

#[uniffi::export]
pub fn parse_job_load(stdout: String) -> Option<JefeLoaded> {
    jefe_core::store::parse_load(&stdout).map(|l| JefeLoaded {
        snapshot: l.snapshot,
        events_json: l
            .events
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect(),
    })
}

/// The job as JSON out of a snapshot, without opening it (for a read-only
/// view of a job another client drives).
#[uniffi::export]
pub fn job_json_of_snapshot(snapshot: String) -> Result<String, JefeError> {
    let v: serde_json::Value = serde_json::from_str(&snapshot).map_err(bad_json)?;
    v.get("job")
        .map(serde_json::Value::to_string)
        .ok_or_else(|| JefeError::Restore {
            reason: "no job in the snapshot".to_owned(),
        })
}

// ---- specs and presets ---------------------------------------------------------

/// A fresh `JobSpec` as JSON: the defaults for `goal` in `repo` on `host`
/// (`local` or the app's own host id). Change it, or `apply_preset`, then
/// `JefeJob::start`.
#[uniffi::export]
pub fn new_job_spec_json(id: String, goal: String, repo: String, host: String, attended: bool) -> String {
    let mut spec = JobSpec::new(&id, &goal, &repo);
    spec.host = jefe_core::HostId(host);
    spec.attended = attended;
    serde_json::to_string(&spec).unwrap_or_default()
}

/// Why a `JobSpec` JSON can't start, if it can't.
#[uniffi::export]
pub fn validate_spec(spec_json: String) -> Option<String> {
    let spec: JobSpec = match serde_json::from_str(&spec_json) {
        Ok(s) => s,
        Err(e) => return Some(format!("invalid JSON: {e}")),
    };
    if !jefe_core::ids::valid_job_id(spec.id.as_str()) {
        return Some("bad job id: up to 40 letters, digits, '-' or '_'".to_owned());
    }
    if spec.goal.trim().is_empty() {
        return Some("say what to get done".to_owned());
    }
    if spec.repo.trim().is_empty() {
        return Some("say which repository".to_owned());
    }
    if spec.budget.max_workers == 0 || spec.budget.max_tasks == 0 {
        return Some("allow at least one worker and one task".to_owned());
    }
    None
}

/// The built-in team preset as JSON (camelCase; the same shape the Sessh
/// apps keep in their settings).
#[uniffi::export]
pub fn default_preset_json() -> String {
    TeamPreset::builtin().to_json()
}

/// A neutral preset (nothing chosen) as JSON, to build one from.
#[uniffi::export]
pub fn empty_preset_json() -> String {
    TeamPreset::default().to_json()
}

/// Why a preset JSON can't run, if it can't.
#[uniffi::export]
pub fn validate_preset(preset_json: String) -> Option<String> {
    match TeamPreset::from_json(&preset_json) {
        Ok(p) => p.validate().err(),
        Err(e) => Some(format!("invalid JSON: {e}")),
    }
}

/// Writes a preset into a `JobSpec` JSON and returns the new spec.
#[uniffi::export]
pub fn apply_preset(spec_json: String, preset_json: String) -> Result<String, JefeError> {
    let mut spec: JobSpec = serde_json::from_str(&spec_json).map_err(bad_json)?;
    let preset = TeamPreset::from_json(&preset_json).map_err(bad_json)?;
    preset
        .validate()
        .map_err(|reason| JefeError::Invalid { reason })?;
    preset.apply(&mut spec);
    serde_json::to_string(&spec).map_err(bad_json)
}

/// A built-in role, for a UI's role table.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JefeRole {
    pub id: String,
    pub description: String,
    /// `read_only`, `report` or `worktree`.
    pub access: String,
}

/// The built-in roles in display order.
#[uniffi::export]
pub fn builtin_roles() -> Vec<JefeRole> {
    jefe_core::preset::builtin_role_configs()
        .into_iter()
        .map(|(id, r)| JefeRole {
            id,
            description: r.description,
            access: r.access.name().to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_job_starts_steps_and_encodes_its_effects() {
        let spec = new_job_spec_json(
            "j1".into(),
            "add hello".into(),
            "/repo".into(),
            "box".into(),
            false,
        );
        assert!(validate_spec(spec.clone()).is_none());
        let spec = apply_preset(spec, default_preset_json()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&spec).unwrap();
        assert_eq!(v["brain"]["kind"], "session");
        assert_eq!(v["workers"]["harness"], "pi");
        let job = JefeJob::start(spec, "phone".into(), 1_000).unwrap();
        let effects = job.take_effects();
        assert!(job.take_effects().is_empty());
        let runs: Vec<serde_json::Value> = effects
            .iter()
            .map(|e| serde_json::from_str::<serde_json::Value>(&e.json).unwrap())
            .filter(|v| v["effect"] == "run")
            .collect();
        let purposes: Vec<&str> = runs
            .iter()
            .map(|r| r["purpose"]["purpose"].as_str().unwrap())
            .collect();
        assert!(
            purposes.contains(&"preflight") && purposes.contains(&"lease"),
            "{purposes:?}"
        );
        assert!(runs.iter().all(|r| r["host"] == "box"));
        let save = effects
            .iter()
            .find(|e| e.json.contains("\"purpose\":\"save\""))
            .unwrap();
        assert!(!save.stdin.is_empty(), "the snapshot travels as stdin");
        assert!(
            !save.json.contains("job_created"),
            "the snapshot stays out of the JSON: {}",
            save.json
        );
        let preflight = runs
            .iter()
            .find(|r| r["purpose"]["purpose"] == "preflight")
            .unwrap();
        let input = serde_json::json!({
            "input": "exec_done", "id": preflight["id"], "code": 0, "stdout": "/repo\nmain\n", "stderr": ""
        });
        let out = job.step(input.to_string(), 1_100).unwrap();
        assert!(out.iter().any(|e| e.json.contains("brain_launch")), "{out:?}");
        assert_eq!(job.status().unwrap(), "planning");
        assert!(job.next_deadline().unwrap().is_some());
        assert!(job.step("nope".into(), 1).is_err());
        assert!(
            job_json_of_snapshot(job.snapshot().unwrap())
                .unwrap()
                .contains("\"goal\":\"add hello\"")
        );
    }

    #[test]
    fn rules_consultations_are_answered_inside() {
        let spec = new_job_spec_json(
            "j2".into(),
            "add hello".into(),
            "/repo".into(),
            "local".into(),
            false,
        );
        let job = JefeJob::start(spec, "phone".into(), 1_000).unwrap();
        let effects = job.take_effects();
        let preflight: serde_json::Value = effects
            .iter()
            .map(|e| serde_json::from_str::<serde_json::Value>(&e.json).unwrap())
            .find(|v| v["purpose"]["purpose"] == "preflight")
            .unwrap();
        let input = serde_json::json!({
            "input": "exec_done", "id": preflight["id"], "code": 0, "stdout": "/repo\nmain\n", "stderr": ""
        });
        let out = job.step(input.to_string(), 1_100).unwrap();
        // The rules planned at once: no consult for the app, a worktree to make.
        assert!(
            !out.iter().any(|e| e.json.contains("\"effect\":\"consult\"")),
            "{out:?}"
        );
        assert!(out.iter().any(|e| e.json.contains("worktree_add")), "{out:?}");
        assert_eq!(job.status().unwrap(), "running");
    }

    #[test]
    fn store_helpers_round_trip_through_strings() {
        let l = parse_lease("ok\tphone\t10\t30\n".into()).unwrap();
        assert!(l.ok && l.holder == "phone");
        assert!(parse_lease("garbage".into()).is_none());
        let jobs = parse_job_list("J\tj1\t-\trunning\t5\tadd hello\n".into());
        assert_eq!(jobs[0].id, "j1");
        assert_eq!(jobs[0].holder, None);
        assert!(
            lease_exec("j1".into(), "phone".into(), 0)
                .script
                .contains("lease")
        );
        assert!(load_job_exec("j1".into(), 0).script.contains("tail -n 200"));
        assert!(validate_preset("{}".into()).is_some(), "a preset needs a name");
        assert!(validate_preset(default_preset_json()).is_none());
        assert_eq!(builtin_roles().len(), 5);
        assert_eq!(jefe_protocol_version(), 1);
    }
}
