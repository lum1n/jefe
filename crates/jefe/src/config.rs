//! `jefe.toml`: every field has a default, unknown fields are errors.

use std::collections::BTreeMap;
use std::path::Path;

use acc_core::Harness;
use acc_core::harness::Launcher;
use acc_core::policy::Rule;
use acc_core::sandbox::Sandbox;
use anyhow::Context as _;
use jefe_core::job::ReviewBy;
use jefe_core::{BrainSpec, Budget, JobSpec, RoleConfig, Unmatched, WorkerConfig};
use serde::{Deserialize, Serialize};

use crate::hosts::{HostConfig, JobHost};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub workers: Workers,
    /// `[brain]` still reads as `[orchestrator]`.
    #[serde(alias = "brain")]
    pub orchestrator: OrchestratorConfig,
    /// `[roles.<id>]`: changes to the built-in roles, or new ones.
    pub roles: BTreeMap<String, RoleConfig>,
    pub limits: Limits,
    pub review: Review,
    pub plan: Plan,
    pub finish: Finish,
    pub merge: Merge,
    /// SSH hosts for `--host <name>`.
    pub hosts: BTreeMap<String, HostConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workers {
    pub harness: Harness,
    pub model: Option<String>,
    /// Reasoning effort, where the harness lists levels.
    pub effort: Option<String>,
    /// Claude permission mode or Codex permission profile.
    pub permission: Option<String>,
    /// Workers running at once.
    pub max: u32,
    /// Harnesses a task is handed to when its own one fails or runs out.
    pub alternates: Vec<Harness>,
    /// acc policy rules added to jefe's default worker policy.
    pub policy: Vec<Rule>,
    /// Run workers in acc's sandbox (`sandbox = {}` for the default preset).
    pub sandbox: Option<Sandbox>,
    /// Wrap or replace the harness executable.
    pub launcher: Option<Launcher>,
    /// Who decides approvals no policy rule matches: deny, brain or human.
    pub unmatched: Unmatched,
}

impl Default for Workers {
    fn default() -> Self {
        let w = WorkerConfig::default();
        Self {
            harness: w.harness,
            model: w.model,
            effort: w.effort,
            permission: w.permission,
            max: Budget::default().max_workers,
            alternates: w.alternates,
            policy: w.policy,
            sandbox: w.sandbox,
            launcher: w.launcher,
            unmatched: w.unmatched,
        }
    }
}

/// Who decides at judgment points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum BrainKind {
    /// jefe's built-in rules.
    #[default]
    Rules,
    /// A fresh harness session for each judgment point (`harness` before 0.2).
    #[serde(alias = "harness")]
    #[value(alias = "harness")]
    Oneshot,
    /// One harness session for the whole job, consulted turn by turn.
    Session,
}

impl From<BrainKind> for jefe_core::BrainKind {
    fn from(k: BrainKind) -> Self {
        match k {
            BrainKind::Rules => Self::Rules,
            BrainKind::Oneshot => Self::Oneshot,
            BrainKind::Session => Self::Session,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestratorConfig {
    pub kind: BrainKind,
    /// Defaults to the workers' harness.
    pub harness: Option<Harness>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Claude permission mode or Codex permission profile for the orchestrator.
    pub permission: Option<String>,
    /// Wrap or replace the orchestrator's harness executable.
    pub launcher: Option<Launcher>,
    pub timeout_secs: u64,
    /// Run the orchestrator in acc's read-only sandbox.
    pub sandbox: bool,
    /// Turns per session before a fresh one is seeded (`session` kind).
    pub max_turns: u32,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        let b = BrainSpec::default();
        Self {
            kind: BrainKind::Rules,
            harness: b.harness,
            model: b.model,
            effort: b.effort,
            permission: b.permission,
            launcher: b.launcher,
            timeout_secs: Budget::default().brain_timeout_ms / 1000,
            sandbox: b.sandbox,
            max_turns: b.max_turns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_tasks: u32,
    pub max_attempts: u32,
    pub max_turns_per_worker: u32,
    pub max_consults: u32,
    pub stall_mins: u64,
    pub wall_mins: u64,
    pub quota_stop_pct: f64,
    /// Fail the job once it has spent this much (harness cost figures).
    pub max_cost_usd: Option<f64>,
}

impl Default for Limits {
    fn default() -> Self {
        let b = Budget::default();
        Self {
            max_tasks: b.max_tasks,
            max_attempts: b.max_attempts,
            max_turns_per_worker: b.max_turns_per_worker,
            max_consults: b.max_consults,
            stall_mins: b.stall_ms / 60_000,
            wall_mins: b.wall_ms / 60_000,
            quota_stop_pct: b.quota_stop_pct,
            max_cost_usd: b.max_cost_usd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    pub required: bool,
    /// Who reviews: `brain`, `reviewer` (a reviewer worker's verdict) or
    /// `both` (the reviewer reads first, the brain decides).
    pub by: ReviewBy,
    pub max_diff_chars: usize,
    pub max_report_chars: usize,
}

impl Default for Review {
    fn default() -> Self {
        Self {
            required: true,
            by: ReviewBy::default(),
            max_diff_chars: 60_000,
            max_report_chars: 20_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Plan {
    /// Wait for the human's approval of the plan. Unset: when attended.
    pub approve: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Finish {
    /// Wait for the human's review of the result before the job ends.
    /// Unset: when attended.
    pub approve: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Merge {
    pub squash: bool,
}

/// The example `jefe config example` prints.
pub const EXAMPLE: &str = r#"# ~/.config/jefe/jefe.toml. Every field is optional; these are the defaults.

[workers]
harness = "claude"
# model = "sonnet"
# effort = "medium"
permission = "acceptEdits"
max = 3
alternates = ["codex"]
# Extra acc policy rules, checked before jefe's defaults:
# policy = [{ decision = "allow", tool = "shell", command = "cargo test" }]
# Run workers in acc's sandbox:
# sandbox = {}
# launcher = { program = "/opt/claude/bin/claude" }
# Approvals no policy rule matches: "deny", "brain" (the orchestrator decides)
# or "human" (asked; denied when nobody is attending).
unmatched = "deny"

# The orchestrator (`[brain]` still works): "rules", "oneshot" (a fresh
# harness session per judgment point) or "session" (one session for the
# whole job, consulted turn by turn; it remembers the plan and your notes).
[orchestrator]
kind = "rules"
# harness = "claude"
# model = "opus"
# effort = "high"
# permission = "default"
# launcher = { program = "/opt/claude/bin/claude" }
timeout_secs = 300
sandbox = true
max_turns = 40

# Roles: explorer, researcher, implementor, tester and reviewer are built in
# and inherit [workers]. Change one, or add your own:
# [roles.explorer]
# harness = "pi"
# model = "opencode-go/deepseek-v4.1-flash"
# effort = "low"
# [roles.tester]
# enabled = false
# [roles.docs]
# description = "writes documentation for a change"
# access = "worktree"            # or "read_only" / "report"
# prompt = "Your role: documenter. ..."

[limits]
max_tasks = 8
max_attempts = 2
max_turns_per_worker = 6
max_consults = 60
stall_mins = 10
wall_mins = 180
quota_stop_pct = 90.0
# max_cost_usd = 10.0

[review]
required = true
by = "both"             # "brain", "reviewer" or "both"
max_diff_chars = 60000
max_report_chars = 20000

[plan]
# approve = true          # wait for your OK before workers start (default: when attended)

[finish]
# approve = true          # review the result before the job ends (default: when attended)

[merge]
squash = false

# Hosts for `--host <name>`, reached over SSH. `local` is this machine.
# [hosts.box]
# host = "box.example.com"
# user = "me"                  # default: $USER
# port = 22
# identity = "~/.ssh/id_ed25519" # an unencrypted key; default: the SSH agent
# known_hosts = "~/.ssh/known_hosts"   # the default, or:
# fingerprints = ["SHA256:..."]
# connect_timeout_secs = 10
"#;

/// A team preset as `jefe.toml` sections, to paste into the config.
pub fn preset_toml(p: &jefe_core::TeamPreset) -> String {
    let mut config = Config::default();
    let mut spec = JobSpec::new("x", "x", "x");
    p.apply(&mut spec);
    config.workers.harness = spec.workers.harness;
    config.workers.model = spec.workers.model.clone();
    config.workers.effort = spec.workers.effort.clone();
    config.workers.max = spec.budget.max_workers;
    config.workers.alternates = spec.workers.alternates.clone();
    config.orchestrator.kind = match spec.brain.kind {
        jefe_core::BrainKind::Rules => BrainKind::Rules,
        jefe_core::BrainKind::Oneshot => BrainKind::Oneshot,
        jefe_core::BrainKind::Session => BrainKind::Session,
    };
    config.orchestrator.harness = spec.brain.harness;
    config.orchestrator.model = spec.brain.model.clone();
    config.orchestrator.effort = spec.brain.effort.clone();
    config.orchestrator.max_turns = spec.brain.max_turns;
    config.roles = spec.roles.clone();
    config.limits.max_tasks = spec.budget.max_tasks;
    config.limits.wall_mins = spec.budget.wall_ms / 60_000;
    config.limits.max_cost_usd = spec.budget.max_cost_usd;
    config.review.by = spec.review_by;
    config.plan.approve = spec.approve_plan;
    config.finish.approve = spec.approve_finish;
    let mut text = format!("# {}\n", p.name);
    text.push_str(&toml::to_string(&config).unwrap_or_default());
    text
}

impl Config {
    /// Reads `path`; a missing file is the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        for (name, h) in &config.hosts {
            if name == "local" {
                anyhow::bail!("[hosts.local]: `local` is this machine and can't be configured");
            }
            if !jefe_core::ids::valid_job_id(name) {
                anyhow::bail!("[hosts.{name}]: use letters, digits, '-' or '_' in host names");
            }
            h.check().with_context(|| format!("[hosts.{name}]"))?;
        }
        for id in config.roles.keys() {
            if !jefe_core::ids::valid_job_id(id) {
                anyhow::bail!("[roles.{id}]: use letters, digits, '-' or '_' in role ids");
            }
        }
        Ok(config)
    }

    /// The host named `name` (`None` or `local`: this machine).
    pub fn job_host(&self, name: Option<&str>) -> anyhow::Result<JobHost> {
        match name {
            None | Some("local") => Ok(JobHost::local()),
            Some(n) => {
                let ssh = self.hosts.get(n).cloned().ok_or_else(|| {
                    let known: Vec<&str> = self.hosts.keys().map(String::as_str).collect();
                    anyhow::anyhow!("no [hosts.{n}] in the config (known: local {})", known.join(" "))
                })?;
                Ok(JobHost {
                    name: jefe_core::HostId(n.to_owned()),
                    ssh: Some(ssh),
                })
            }
        }
    }

    pub fn worker_config(&self) -> WorkerConfig {
        let w = &self.workers;
        WorkerConfig {
            harness: w.harness,
            model: w.model.clone(),
            effort: w.effort.clone(),
            permission: w.permission.clone(),
            alternates: w.alternates.iter().copied().filter(|h| *h != w.harness).collect(),
            policy: w.policy.clone(),
            sandbox: w.sandbox.clone(),
            launcher: w.launcher.clone(),
            unmatched: w.unmatched,
        }
    }

    pub fn brain_spec(&self) -> BrainSpec {
        let o = &self.orchestrator;
        BrainSpec {
            kind: o.kind.into(),
            harness: o.harness,
            model: o.model.clone(),
            effort: o.effort.clone(),
            permission: o.permission.clone(),
            launcher: o.launcher.clone(),
            sandbox: o.sandbox,
            max_turns: o.max_turns.max(1),
        }
    }

    pub fn budget(&self) -> Budget {
        let l = &self.limits;
        Budget {
            max_workers: self.workers.max.max(1),
            max_tasks: l.max_tasks,
            max_attempts: l.max_attempts,
            max_turns_per_worker: l.max_turns_per_worker,
            max_consults: l.max_consults,
            stall_ms: l.stall_mins.saturating_mul(60_000),
            wall_ms: l.wall_mins.saturating_mul(60_000),
            brain_timeout_ms: self.orchestrator.timeout_secs.saturating_mul(1000),
            quota_stop_pct: l.quota_stop_pct,
            review_required: self.review.required,
            max_cost_usd: l.max_cost_usd,
        }
    }

    /// A job spec with everything the config says; the caller sets the
    /// goal, repository, host and attendance.
    pub fn job_spec(&self, id: &str, goal: &str, repo: &str) -> JobSpec {
        let mut spec = JobSpec::new(id, goal, repo);
        spec.workers = self.worker_config();
        spec.roles = self.roles.clone();
        spec.brain = self.brain_spec();
        spec.budget = self.budget();
        spec.approve_plan = self.plan.approve;
        spec.approve_finish = self.finish.approve;
        spec.review_by = self.review.by;
        spec.squash = self.merge.squash;
        spec.max_diff_chars = self.review.max_diff_chars;
        spec.max_report_chars = self.review.max_report_chars;
        spec
    }

    /// Every role id a repository may override prompts for.
    pub fn role_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = jefe_core::job::builtin_roles().into_keys().collect();
        for id in self.roles.keys() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn example_is_the_defaults() {
        assert_eq!(Config::parse(EXAMPLE).unwrap(), Config::default());
    }

    #[test]
    fn unknown_fields_are_errors() {
        assert!(Config::parse("[workers]\nharnes = \"claude\"\n").is_err());
        assert!(Config::parse("[nope]\n").is_err());
    }

    #[test]
    fn hosts_are_looked_up_by_name() {
        let c = Config::parse("[hosts.box]\nhost = \"10.0.0.2\"\nport = 2200\n").unwrap();
        assert!(c.job_host(None).unwrap().is_local());
        assert!(c.job_host(Some("local")).unwrap().is_local());
        let h = c.job_host(Some("box")).unwrap();
        assert_eq!(h.name.as_str(), "box");
        assert_eq!(h.ssh.unwrap().port, 2200);
        assert!(c.job_host(Some("nope")).is_err());
        assert!(Config::parse("[hosts.local]\nhost = \"x\"\n").is_err());
        assert!(Config::parse("[hosts.box]\nhost = \"x\"\nnope = 1\n").is_err());
    }

    #[test]
    fn unmatched_reaches_the_workers() {
        let c = Config::parse("[workers]\nunmatched = \"brain\"\n").unwrap();
        assert_eq!(c.worker_config().unmatched, Unmatched::Brain);
        assert!(Config::parse("[workers]\nunmatched = \"maybe\"\n").is_err());
    }

    #[test]
    fn sandbox_table_turns_it_on() {
        let c = Config::parse("[workers]\nsandbox = {}\n").unwrap();
        assert_eq!(c.worker_config().sandbox, Some(Sandbox::default()));
    }

    #[test]
    fn brain_section_and_harness_kind_still_read() {
        let c = Config::parse("[brain]\nkind = \"harness\"\nmodel = \"opus\"\n").unwrap();
        assert_eq!(c.orchestrator.kind, BrainKind::Oneshot);
        let spec = c.job_spec("j", "g", "/r");
        assert_eq!(spec.brain.kind, jefe_core::BrainKind::Oneshot);
        assert_eq!(spec.brain.model.as_deref(), Some("opus"));
        let c = Config::parse("[orchestrator]\nkind = \"session\"\nmax_turns = 3\n").unwrap();
        assert_eq!(c.brain_spec().kind, jefe_core::BrainKind::Session);
        assert_eq!(c.brain_spec().max_turns, 3);
    }

    #[test]
    fn the_builtin_preset_prints_as_a_valid_config() {
        let text = preset_toml(&jefe_core::TeamPreset::builtin());
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.orchestrator.kind, BrainKind::Session);
        assert_eq!(c.workers.harness, Harness::Pi);
        assert_eq!(c.roles["explorer"].effort.as_deref(), Some("low"));
    }

    #[test]
    fn roles_review_and_plan_reach_the_spec() {
        let c = Config::parse(
            "[roles.explorer]\nharness = \"pi\"\neffort = \"low\"\n[roles.tester]\nenabled = false\n\
             [review]\nby = \"reviewer\"\n[plan]\napprove = true\n[finish]\napprove = false\n[limits]\nmax_cost_usd = 2.5\n",
        )
        .unwrap();
        let spec = c.job_spec("j", "g", "/r");
        assert_eq!(spec.resolve("explorer").unwrap().harness, Harness::Pi);
        assert!(spec.resolve("tester").is_none());
        assert_eq!(spec.review_by, ReviewBy::Reviewer);
        assert_eq!(spec.approve_plan, Some(true));
        assert_eq!(spec.approve_finish, Some(false));
        assert_eq!(spec.budget.max_cost_usd, Some(2.5));
        assert!(Config::parse("[roles.bad id]\nenabled = false\n").is_err());
    }
}
