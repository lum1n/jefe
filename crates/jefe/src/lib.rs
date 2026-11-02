//! jefe as a library: the host side of the supervision loop.

pub mod brain;
pub mod config;
pub mod driver;
pub mod hosts;
pub mod output;
pub mod paths;
pub mod store;

use std::sync::Arc;

use acc_core::session::SessionSpec;
use anyhow::Context as _;
use jefe_core::prompts::Prompts;
use jefe_core::{HumanReply, Input, Jefe, JobSpec, Report};
use serde::Serialize;

pub use brain::HarnessBrain;
pub use driver::{BrainRunner, Control, Supervisor};
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
        brains: vec!["rules", "harness"],
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

/// Overrides from `<repo>/.jefe/prompts/{worker,chief}.md`.
pub fn repo_prompts(repo: &std::path::Path) -> Prompts {
    let mut p = Prompts::default();
    let dir = repo.join(".jefe/prompts");
    if let Ok(text) = std::fs::read_to_string(dir.join("worker.md")) {
        p.worker = text;
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("chief.md")) {
        p.chief = text;
    }
    p
}

fn runner(brain: Option<HarnessBrain>) -> BrainRunner {
    match brain {
        Some(b) => BrainRunner::Harness(Arc::new(b)),
        None => BrainRunner::Rules,
    }
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
    brain: Option<HarnessBrain>,
    mode: Mode,
    control: bool,
) -> anyhow::Result<Report> {
    let (sup, effects) = prepare_start(spec, host, brain, |job| Output::new(mode, job)).await?;
    sup.run(effects, Vec::new(), control).await
}

/// Like [`start`], but nothing is printed and no stdin or signals are read:
/// messages come on [`Embedded::messages`] and control goes through
/// [`Embedded::control`]. Job state is still kept under `JEFE_STATE_DIR`.
pub async fn start_embedded(
    spec: JobSpec,
    host: JobHost,
    brain: Option<HarnessBrain>,
) -> anyhow::Result<Embedded> {
    let (tx, messages) = tokio::sync::mpsc::unbounded_channel();
    let (sup, effects) = prepare_start(spec, host, brain, |job| Output::embedded(job, tx)).await?;
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
    brain: Option<HarnessBrain>,
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
    store.save_brain(brain.as_ref())?;
    store.save_host(&host)?;
    let out = out(spec.id.clone());
    let (jefe, effects) = Jefe::new(spec, driver::now_ms());
    Ok((Supervisor::new(jefe, conn, store, out, runner(brain)), effects))
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
    let snapshot = store::load(id)?;
    let job_host = store::load_host(id)?;
    let host = job_host.open()?;
    let specs: Vec<SessionSpec> = acc_ssh::list(&host)
        .await
        .with_context(|| format!("listing acc sessions on host {}", job_host.name))?
        .iter()
        .filter_map(|l| SessionSpec::from_json(l.spec_json.as_deref()?))
        .collect();
    let (jefe, effects) = Jefe::restore(&snapshot, &specs, driver::now_ms())?;
    let out = out(jefe.job().id.clone());
    if jefe.is_finished() {
        let report = jefe
            .job()
            .report
            .clone()
            .ok_or_else(|| anyhow::anyhow!("job {id} finished without a report"))?;
        out.finished(&report);
        return Ok(Resumed::Over(report));
    }
    let store = Store::open(id)?;
    let first = if cancel {
        vec![Input::Human(HumanReply::Cancel)]
    } else {
        Vec::new()
    };
    let brain = store::load_brain(id)?;
    Ok(Resumed::Running(
        Box::new(Supervisor::new(jefe, host, store, out, runner(brain))),
        effects,
        first,
    ))
}
