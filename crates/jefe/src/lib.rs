//! jefe as a library: the host side of the supervision loop.

pub mod config;
pub mod driver;
pub mod hosts;
pub mod output;
pub mod paths;
pub mod store;

use acc_core::session::SessionSpec;
use anyhow::Context as _;
use jefe_core::prompts::Prompts;
use jefe_core::{Hosted, HumanReply, Input, Jefe, JobSpec, Report};
use serde::Serialize;

pub use driver::{Control, Supervisor};
pub use hosts::{HostConfig, JobHost};
pub use output::{Message, Mode, Output};
pub use store::Store;

/// What this build supports, for `jefe capabilities --json`.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Protocol major version.
    pub version: u32,
    pub jefe: &'static str,
    pub acc: &'static str,
    /// Harness names as jefe's config and `--harness` take them.
    pub harnesses: Vec<String>,
    pub brains: Vec<&'static str>,
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        kind: "capabilities",
        version: jefe_core::PROTOCOL_VERSION,
        jefe: env!("CARGO_PKG_VERSION"),
        acc: acc_core::VERSION,
        harnesses: acc_core::Harness::ALL
            .iter()
            .filter_map(|h| serde_json::to_value(h).ok()?.as_str().map(str::to_owned))
            .collect(),
        brains: vec!["rules", "oneshot", "session"],
    }
}

/// A fresh job id: the time in base 36, made unique among existing jobs.
pub fn new_job_id() -> String {
    let mut n = driver::now_ms() / 1000;
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
        n /= 36;
    }
    let base: String = digits.into_iter().rev().collect();
    let mut id = base.clone();
    let mut i = 2;
    while Store::dir_for(&id).exists() {
        id = format!("{base}-{i}");
        i += 1;
    }
    id
}

/// Overrides from `<repo>/.jefe/prompts/{worker,chief,chief_turn}.md` and
/// `roles/<role>.md` in a local repository.
pub fn repo_prompts(repo: &std::path::Path, roles: &[String]) -> Prompts {
    let mut p = Prompts::default();
    let dir = repo.join(".jefe/prompts");
    if let Ok(text) = std::fs::read_to_string(dir.join("worker.md")) {
        p.worker = text;
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("chief.md")) {
        p.chief = text;
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("chief_turn.md")) {
        p.chief_turn = text;
    }
    for role in roles {
        if let Ok(text) = std::fs::read_to_string(dir.join("roles").join(format!("{role}.md"))) {
            p.roles.insert(role.clone(), text);
        }
    }
    p
}

/// The same overrides read from `host`, for repositories this machine
/// can't read directly.
pub async fn host_prompts(host: &acc_ssh::Host, repo: &str, roles: &[String]) -> anyhow::Result<Prompts> {
    let mut p = Prompts::default();
    let read = |name: String| {
        let q = acc_core::shell::quote(&format!("{repo}/.jefe/prompts/{name}.md"));
        // A leading `y` tells a present (maybe empty) file from a missing one.
        format!("if [ -f {q} ]; then printf y; cat {q}; fi")
    };
    let mut files: Vec<(String, Option<String>)> = vec![
        ("worker".to_owned(), None),
        ("chief".to_owned(), None),
        ("chief_turn".to_owned(), None),
    ];
    files.extend(roles.iter().map(|r| (format!("roles/{r}"), Some(r.clone()))));
    for (name, role) in files {
        let out = host
            .exec(&read(name.clone()), &[])
            .await
            .and_then(acc_ssh::Output::ok)
            .with_context(|| format!("reading {name}.md on the host"))?;
        if let Some(text) = out.stdout_str().strip_prefix('y') {
            match (name.as_str(), role) {
                ("worker", _) => p.worker = text.to_owned(),
                ("chief", _) => p.chief = text.to_owned(),
                ("chief_turn", _) => p.chief_turn = text.to_owned(),
                (_, Some(role)) => {
                    p.roles.insert(role, text.to_owned());
                }
                _ => {}
            }
        }
    }
    Ok(p)
}

/// Where a job's state lives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum StateMode {
    /// This machine's `JEFE_STATE_DIR` only.
    #[default]
    Local,
    /// Also on the job's host, with a lease, so another client (a desktop
    /// or a phone) can pick the job up.
    Host,
}

/// This process's name in a host-side lease.
fn client_name() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "cli".to_owned());
    format!("jefe-{host}-{}", std::process::id())
}

/// A job driven inside the embedding program's tokio runtime.
#[derive(Debug)]
pub struct Embedded {
    /// Every protocol message, in order; it ends after `bye`.
    pub messages: tokio::sync::mpsc::UnboundedReceiver<Message>,
    /// Answers, nudges, cancel and status requests.
    pub control: Control,
    /// The job's report, once it is over.
    pub done: tokio::task::JoinHandle<anyhow::Result<Report>>,
}

/// Starts a new job on `host` (whose name must be `spec.host`) and
/// supervises it to the end, printing to stdout.
pub async fn start(
    spec: JobSpec,
    host: JobHost,
    state: StateMode,
    mode: Mode,
    control: bool,
) -> anyhow::Result<Report> {
    let (sup, effects) = prepare_start(spec, host, state, |job| Output::new(mode, job)).await?;
    sup.run(effects, Vec::new(), control).await
}

/// Like [`start`], but nothing is printed and no stdin or signals are read:
/// messages come on [`Embedded::messages`] and control goes through
/// [`Embedded::control`]. Job state is still kept under `JEFE_STATE_DIR`.
pub async fn start_embedded(spec: JobSpec, host: JobHost, state: StateMode) -> anyhow::Result<Embedded> {
    let (tx, messages) = tokio::sync::mpsc::unbounded_channel();
    let (sup, effects) = prepare_start(spec, host, state, |job| Output::embedded(job, tx)).await?;
    let sup = sup.without_signals();
    let control = sup.control();
    let done = tokio::spawn(sup.run(effects, Vec::new(), false));
    Ok(Embedded {
        messages,
        control,
        done,
    })
}

async fn prepare_start(
    spec: JobSpec,
    host: JobHost,
    state: StateMode,
    out: impl FnOnce(jefe_core::JobId) -> Output,
) -> anyhow::Result<(Supervisor, Vec<jefe_core::Effect>)> {
    anyhow::ensure!(
        spec.host == host.name,
        "job host {} is not {}",
        spec.host,
        host.name
    );
    if store::Store::dir_for(spec.id.as_str()).join("job.json").exists() {
        anyhow::bail!("job {} exists; resume it instead", spec.id);
    }
    let conn = host.open()?;
    conn.connect()
        .await
        .with_context(|| format!("connecting to host {}", host.name))?;
    let store = Store::open(spec.id.as_str())?;
    store.save_host(&host)?;
    store.save_state(state)?;
    let store = if state == StateMode::Host && host.is_local() {
        store.without_mirror()
    } else {
        store
    };
    let out = out(spec.id.clone());
    let now = driver::now_ms();
    let (jefe, effects) = match state {
        StateMode::Local => {
            let (jefe, effects) = Jefe::new(spec, now);
            (Hosted::local(jefe, now), effects)
        }
        StateMode::Host => Hosted::start(spec, &client_name(), now),
    };
    Ok((Supervisor::new(jefe, conn, store, out), effects))
}

/// Picks job `id` up where its last jefe process left it, reattaching to
/// its workers' acc sessions. With `cancel`, stops it instead.
pub async fn resume(id: &str, mode: Mode, control: bool, cancel: bool) -> anyhow::Result<Report> {
    match prepare_resume(id, cancel, |job| Output::new(mode, job)).await? {
        Resumed::Over(report) => Ok(report),
        Resumed::Running(sup, effects, first) => sup.run(effects, first, control).await,
    }
}

/// [`resume`] for an embedding program, as [`start_embedded`] is for [`start`].
pub async fn resume_embedded(id: &str) -> anyhow::Result<Embedded> {
    let (tx, messages) = tokio::sync::mpsc::unbounded_channel();
    match prepare_resume(id, false, |job| Output::embedded(job, tx)).await? {
        Resumed::Over(report) => {
            // Nothing to drive; the `bye` is already in `messages`.
            let control = Control::detached();
            Ok(Embedded {
                messages,
                control,
                done: tokio::spawn(async move { Ok(report) }),
            })
        }
        Resumed::Running(sup, effects, first) => {
            let sup = (*sup).without_signals();
            let control = sup.control();
            let done = tokio::spawn(sup.run(effects, first, false));
            Ok(Embedded {
                messages,
                control,
                done,
            })
        }
    }
}

enum Resumed {
    Over(Report),
    Running(Box<Supervisor>, Vec<jefe_core::Effect>, Vec<Input>),
}

async fn prepare_resume(
    id: &str,
    cancel: bool,
    out: impl FnOnce(jefe_core::JobId) -> Output,
) -> anyhow::Result<Resumed> {
    if let Some(pid) = store::driver_pid(id) {
        anyhow::bail!("job {id} is being driven by jefe process {pid}");
    }
    let state = store::load_state(id)?;
    let job_host = store::load_host(id)?;
    let host = job_host.open()?;
    let now = driver::now_ms();
    let client = client_name();
    // With host-side state the host's copy is the truth: take the lease,
    // then read the snapshot there.
    let snapshot = match state {
        StateMode::Local => store::load(id)?,
        StateMode::Host => {
            let job = jefe_core::JobId(id.to_owned());
            let lease = jefe_core::store::lease_exec(&job, &client, jefe_core::store::LEASE_TTL_MS);
            let out = host
                .exec(&lease.script, &lease.stdin)
                .await
                .and_then(acc_ssh::Output::ok)
                .with_context(|| format!("taking job {id}'s lease on host {}", job_host.name))?;
            match jefe_core::store::parse_lease(&out.stdout_str()) {
                Some(l) if l.ok => {}
                Some(l) => anyhow::bail!(
                    "job {id} is being driven by {} (lease taken {} s ago)",
                    l.holder,
                    0
                ),
                None => anyhow::bail!("job {id}: the host gave no lease answer"),
            }
            let load = jefe_core::store::load_exec(&job, jefe_core::store::LOAD_TAIL);
            let out = host
                .exec(&load.script, &load.stdin)
                .await
                .and_then(acc_ssh::Output::ok)
                .with_context(|| format!("reading job {id} on host {}", job_host.name))?;
            jefe_core::store::parse_load(&out.stdout_str())
                .map(|l| l.snapshot)
                .ok_or_else(|| {
                    anyhow::anyhow!("job {id} has no readable snapshot on host {}", job_host.name)
                })?
        }
    };
    let specs: Vec<SessionSpec> = acc_ssh::list(&host)
        .await
        .with_context(|| format!("listing acc sessions on host {}", job_host.name))?
        .iter()
        .filter_map(|l| SessionSpec::from_json(l.spec_json.as_deref()?))
        .collect();
    let (jefe, effects) = match state {
        StateMode::Local => {
            let (jefe, effects) = Jefe::restore(&snapshot, &specs, now)?;
            (Hosted::local(jefe, now), effects)
        }
        StateMode::Host => Hosted::open(&snapshot, &specs, &client, now)?,
    };
    let out = out(jefe.jefe().job().id.clone());
    if jefe.jefe().is_finished() {
        let report = jefe
            .jefe()
            .job()
            .report
            .clone()
            .ok_or_else(|| anyhow::anyhow!("job {id} finished without a report"))?;
        out.finished(&report);
        return Ok(Resumed::Over(report));
    }
    let store = Store::open(id)?;
    let store = if state == StateMode::Host && job_host.is_local() {
        store.without_mirror()
    } else {
        store
    };
    let first = if cancel {
        vec![Input::Human(HumanReply::Cancel)]
    } else {
        Vec::new()
    };
    Ok(Resumed::Running(
        Box::new(Supervisor::new(jefe, host, store, out)),
        effects,
        first,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn roles() -> Vec<String> {
        crate::config::Config::default().role_ids()
    }

    #[test]
    fn repo_prompts_default_without_overrides() {
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(repo_prompts(repo.path(), &roles()), Prompts::default());
    }

    #[test]
    fn repo_prompts_override_each_prompt_independently() {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path().join(".jefe/prompts");
        std::fs::create_dir_all(dir.join("roles")).unwrap();
        std::fs::write(dir.join("worker.md"), "my worker").unwrap();
        std::fs::write(dir.join("roles/tester.md"), "my tester").unwrap();
        let p = repo_prompts(repo.path(), &roles());
        assert_eq!(p.worker, "my worker");
        assert_eq!(p.chief, Prompts::default().chief);
        assert_eq!(p.chief_turn, Prompts::default().chief_turn);
        assert_eq!(p.roles.get("tester").map(String::as_str), Some("my tester"));
        assert!(!p.roles.contains_key("explorer"));

        // An empty file is still an override.
        std::fs::write(dir.join("chief.md"), "").unwrap();
        std::fs::write(dir.join("chief_turn.md"), "turn").unwrap();
        let p = repo_prompts(repo.path(), &roles());
        assert_eq!(p.chief, "");
        assert_eq!(p.chief_turn, "turn");
    }
}
