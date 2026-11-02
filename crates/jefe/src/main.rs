//! The `jefe` command.

use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use jefe::config::{BrainKind, Config};
use jefe::{Mode, store};
use jefe_core::{HostId, JobId, JobSpec};

#[derive(Debug, Parser)]
#[command(
    name = "jefe",
    version,
    about = "Delegate a goal to coding agents and supervise them",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// What to get done.
    #[arg(required = true)]
    goal: Option<String>,
    /// The repository (default: the current directory).
    #[arg(long)]
    repo: Option<PathBuf>,
    /// The branch work merges into; it must be checked out.
    #[arg(long)]
    base: Option<String>,
    /// The workers' harness (claude, codex, cursor, copilot, opencode, pi).
    #[arg(long)]
    harness: Option<String>,
    #[arg(long, value_enum)]
    brain: Option<BrainKind>,
    /// Workers running at once.
    #[arg(long)]
    workers: Option<u32>,
    /// Merge what workers report done without reviewing it.
    #[arg(long)]
    no_review: bool,
    /// Nobody answers questions: never wait on a human.
    #[arg(long)]
    unattended: bool,
    /// Start even with uncommitted changes in the repository.
    #[arg(long)]
    allow_dirty: bool,
    /// The job's id (default: one made from the time).
    #[arg(long)]
    id: Option<String>,
    /// Speak the NDJSON protocol instead of prose.
    #[arg(long)]
    json: bool,
    #[arg(long, env = "JEFE_CONFIG")]
    config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start a job (the same as `jefe "<goal>"`).
    Run(RunArgs),
    /// Show a job's state (default: the latest).
    Status {
        job: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List jobs, newest first.
    Jobs {
        #[arg(long)]
        json: bool,
    },
    /// Resume supervising a job whose jefe process is gone.
    Attach {
        job: String,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a job, keeping its branches.
    Stop { job: String },
    /// What this build of jefe supports.
    Capabilities {
        #[arg(long)]
        json: bool,
    },
    /// Print an example config, or check the current one.
    Config {
        #[command(subcommand)]
        what: ConfigCmd,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    Example,
    Check {
        #[arg(long, env = "JEFE_CONFIG")]
        config: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jefe: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn mode(json: bool) -> Mode {
    if json { Mode::Json } else { Mode::Prose }
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    let command = match cli.command {
        Some(c) => c,
        None => Command::Run(cli.run),
    };
    match command {
        Command::Run(args) => run_job(args),
        Command::Status { job, json } => status(job, json),
        Command::Jobs { json } => jobs(json),
        Command::Attach { job, json } => {
            let report = runtime()?.block_on(jefe::resume(&job, mode(json), true, false))?;
            Ok(jefe::output::exit_code(report.status))
        }
        Command::Stop { job } => stop(&job),
        Command::Capabilities { json } => {
            let caps = jefe::capabilities();
            if json {
                println!("{}", serde_json::to_string(&caps)?);
            } else {
                println!("jefe {} (protocol {}, acc {})", caps.jefe, caps.version, caps.acc);
                println!("harnesses: {}", caps.harnesses.join(", "));
                println!("brains: {}", caps.brains.join(", "));
            }
            Ok(0)
        }
        Command::Config { what } => match what {
            ConfigCmd::Example => {
                print!("{}", jefe::config::EXAMPLE);
                Ok(0)
            }
            ConfigCmd::Check { config } => {
                let path = config.unwrap_or_else(jefe::paths::config_file);
                Config::load(&path)?;
                println!("{}: ok", path.display());
                Ok(0)
            }
        },
    }
}

fn run_job(a: RunArgs) -> anyhow::Result<i32> {
    let goal = a.goal.unwrap_or_default();
    if goal.trim().is_empty() {
        anyhow::bail!("say what to get done: jefe \"<goal>\"");
    }
    let config = Config::load(&a.config.unwrap_or_else(jefe::paths::config_file))?;
    if a.brain.unwrap_or(config.brain.kind) == BrainKind::Harness {
        anyhow::bail!("the harness brain isn't available yet; use --brain rules");
    }
    let repo = a.repo.unwrap_or_else(|| PathBuf::from("."));
    let repo = repo
        .canonicalize()
        .with_context(|| format!("no repository at {}", repo.display()))?;
    let id = a.id.unwrap_or_else(jefe::new_job_id);
    if !jefe_core::ids::valid_job_id(&id) {
        anyhow::bail!("bad job id {id:?}: use up to 40 letters, digits, '-' or '_'");
    }
    if store::Store::dir_for(&id).join("job.json").exists() {
        anyhow::bail!("job {id} exists; `jefe attach {id}` resumes it");
    }
    let mut workers = config.worker_config();
    if let Some(h) = a.harness {
        workers.harness = serde_json::from_value(serde_json::Value::String(h.clone()))
            .map_err(|_| anyhow::anyhow!("unknown harness {h:?}"))?;
        workers.alternates.retain(|x| *x != workers.harness);
    }
    let mut budget = config.budget();
    if let Some(n) = a.workers {
        budget.max_workers = n.max(1);
    }
    if a.no_review {
        budget.review_required = false;
    }
    let attended = !a.unattended && (a.json || std::io::stdin().is_terminal());
    let repo_str = repo.to_string_lossy().into_owned();
    let spec = JobSpec {
        id: JobId(id),
        goal,
        repo: repo_str,
        base: a.base,
        host: HostId::local(),
        workers,
        budget,
        attended,
        allow_dirty: a.allow_dirty,
        squash: config.merge.squash,
        max_diff_chars: config.review.max_diff_chars,
        prompts: jefe::repo_prompts(&repo),
    };
    let report = runtime()?.block_on(jefe::start(spec, mode(a.json), attended))?;
    Ok(jefe::output::exit_code(report.status))
}

fn latest(job: Option<String>) -> anyhow::Result<String> {
    job.or_else(|| store::job_ids().into_iter().next())
        .ok_or_else(|| anyhow::anyhow!("no jobs yet"))
}

fn read_job(id: &str) -> anyhow::Result<serde_json::Value> {
    let snap: serde_json::Value = serde_json::from_str(&store::load(id)?)?;
    Ok(snap["job"].clone())
}

fn str_of(v: &serde_json::Value) -> &str {
    v.as_str().unwrap_or("")
}

fn status(job: Option<String>, json: bool) -> anyhow::Result<i32> {
    let id = latest(job)?;
    let job = read_job(&id)?;
    let driver = store::driver_pid(&id);
    if json {
        let line = serde_json::json!({ "type": "snapshot", "job": job, "driver_pid": driver });
        println!("{line}");
        return Ok(0);
    }
    let live = if driver.is_some() {
        ""
    } else {
        " (no jefe attached)"
    };
    println!("job {id}: {}{live}", str_of(&job["status"]));
    println!("goal: {}", str_of(&job["goal"]));
    for t in job["tasks"].as_array().into_iter().flatten() {
        let worker = t["worker"].as_str().map(|w| format!(", {w}")).unwrap_or_default();
        println!(
            "  {} [{}{worker}] {}",
            str_of(&t["id"]),
            str_of(&t["status"]),
            str_of(&t["title"])
        );
    }
    for w in job["workers"].as_array().into_iter().flatten() {
        if w["discarded"].as_bool() == Some(true) {
            continue;
        }
        println!("  {}: {}", str_of(&w["id"]), str_of(&w["phase"]["phase"]));
    }
    Ok(0)
}

fn jobs(json: bool) -> anyhow::Result<i32> {
    for id in store::job_ids() {
        let Ok(job) = read_job(&id) else { continue };
        if json {
            let line = serde_json::json!({
                "type": "job",
                "job": id,
                "status": job["status"],
                "goal": job["goal"],
                "driver_pid": store::driver_pid(&id),
            });
            println!("{line}");
        } else {
            println!("{id}  {:<10} {}", str_of(&job["status"]), str_of(&job["goal"]));
        }
    }
    Ok(0)
}

fn stop(id: &str) -> anyhow::Result<i32> {
    match store::driver_pid(id) {
        Some(pid) => {
            let ok = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                anyhow::bail!("couldn't signal jefe process {pid}");
            }
            println!("asked jefe process {pid} to stop job {id}");
            Ok(0)
        }
        None => {
            let report = runtime()?.block_on(jefe::resume(id, Mode::Prose, false, true))?;
            Ok(jefe::output::exit_code(report.status))
        }
    }
}
