//! `jefe.toml`: every field has a default, unknown fields are errors.

use std::collections::BTreeMap;
use std::path::Path;

use acc_core::Harness;
use acc_core::harness::Launcher;
use acc_core::policy::Rule;
use acc_core::sandbox::Sandbox;
use anyhow::Context as _;
use jefe_core::{Budget, WorkerConfig};
use serde::{Deserialize, Serialize};

use crate::hosts::{HostConfig, JobHost};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub workers: Workers,
    pub brain: BrainConfig,
    pub limits: Limits,
    pub review: Review,
    pub merge: Merge,
    /// SSH hosts for `--host <name>`.
    pub hosts: BTreeMap<String, HostConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workers {
    pub harness: Harness,
    pub model: Option<String>,
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
}

impl Default for Workers {
    fn default() -> Self {
        let w = WorkerConfig::default();
        Self {
            harness: w.harness,
            model: w.model,
            permission: w.permission,
            max: Budget::default().max_workers,
            alternates: w.alternates,
            policy: w.policy,
            sandbox: w.sandbox,
            launcher: w.launcher,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum BrainKind {
    /// jefe's built-in rules.
    #[default]
    Rules,
    /// A one-shot turn on a harness.
    Harness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrainConfig {
    pub kind: BrainKind,
    /// Defaults to the workers' harness.
    pub harness: Option<Harness>,
    pub model: Option<String>,
    /// Claude permission mode or Codex permission profile for the brain.
    pub permission: Option<String>,
    /// Wrap or replace the brain's harness executable.
    pub launcher: Option<Launcher>,
    pub timeout_secs: u64,
    /// Run the brain in acc's read-only sandbox.
    pub sandbox: bool,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            kind: BrainKind::Rules,
            harness: None,
            model: None,
            permission: None,
            launcher: None,
            timeout_secs: Budget::default().brain_timeout_ms / 1000,
            sandbox: true,
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    pub required: bool,
    pub max_diff_chars: usize,
}

impl Default for Review {
    fn default() -> Self {
        Self {
            required: true,
            max_diff_chars: 60_000,
        }
    }
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
permission = "acceptEdits"
max = 3
alternates = ["codex"]
# Extra acc policy rules, checked before jefe's defaults:
# policy = [{ decision = "allow", tool = "shell", command = "cargo test" }]
# Run workers in acc's sandbox:
# sandbox = {}
# launcher = { program = "/opt/claude/bin/claude" }

[brain]
kind = "rules"          # or "harness"
# harness = "claude"
# model = "opus"
# permission = "default"
# launcher = { program = "/opt/claude/bin/claude" }
timeout_secs = 300
sandbox = true

[limits]
max_tasks = 8
max_attempts = 2
max_turns_per_worker = 6
max_consults = 60
stall_mins = 10
wall_mins = 180
quota_stop_pct = 90.0

[review]
required = true
max_diff_chars = 60000

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
            permission: w.permission.clone(),
            alternates: w.alternates.iter().copied().filter(|h| *h != w.harness).collect(),
            policy: w.policy.clone(),
            sandbox: w.sandbox.clone(),
            launcher: w.launcher.clone(),
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
            brain_timeout_ms: self.brain.timeout_secs.saturating_mul(1000),
            quota_stop_pct: l.quota_stop_pct,
            review_required: self.review.required,
        }
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
    fn sandbox_table_turns_it_on() {
        let c = Config::parse("[workers]\nsandbox = {}\n").unwrap();
        assert_eq!(c.worker_config().sandbox, Some(Sandbox::default()));
    }
}
