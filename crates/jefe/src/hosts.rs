//! Where a job's workers run: this machine, or a host from `[hosts.<name>]`
//! reached over SSH with acc-ssh. One host per job.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use acc_ssh::{AgentSocket, Auth, Host, HostKeyCheck, SshConfig};
use anyhow::Context as _;
use jefe_core::HostId;
use serde::{Deserialize, Serialize};

/// `[hosts.<name>]` in `jefe.toml`. Holds paths, never secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Host name or address to dial.
    pub host: String,
    /// Default: `$USER`.
    pub user: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    /// An unencrypted private key file. Default: the SSH agent.
    pub identity: Option<PathBuf>,
    /// Trust only these host key fingerprints (`SHA256:…`, as `ssh-keygen -lf` prints them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprints: Vec<String>,
    /// Trust the keys an OpenSSH `known_hosts` file lists for the host.
    /// Default: `~/.ssh/known_hosts`, unless `fingerprints` is set.
    pub known_hosts: Option<PathBuf>,
    /// Limit for dialing and authenticating.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
}

fn default_port() -> u16 {
    22
}

fn default_connect_timeout() -> u64 {
    10
}

impl HostConfig {
    pub fn check(&self) -> anyhow::Result<()> {
        if self.host.trim().is_empty() {
            anyhow::bail!("host is empty");
        }
        if !self.fingerprints.is_empty() && self.known_hosts.is_some() {
            anyhow::bail!("set fingerprints or known_hosts, not both");
        }
        Ok(())
    }

    /// The acc-ssh connection settings, reading the identity file if one is set.
    pub fn ssh_config(&self) -> anyhow::Result<SshConfig> {
        self.check()?;
        let user = match &self.user {
            Some(u) => u.clone(),
            None => std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .context("no user set and $USER is empty")?,
        };
        let auth = match &self.identity {
            Some(path) => {
                let path = expand(path);
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading identity {}", path.display()))?;
                Auth::key(&text, None).with_context(|| {
                    format!(
                        "loading identity {} (encrypted keys go through the SSH agent: drop `identity`)",
                        path.display()
                    )
                })?
            }
            None => Auth::Agent(AgentSocket::Env),
        };
        let host_key = if self.fingerprints.is_empty() {
            let path = self
                .known_hosts
                .as_deref()
                .map_or_else(|| home().join(".ssh/known_hosts"), expand);
            HostKeyCheck::KnownHosts(path)
        } else {
            HostKeyCheck::Fingerprints(self.fingerprints.clone())
        };
        let mut config = SshConfig::new(self.host.clone(), user, auth, host_key);
        config.port = self.port;
        config.connect_timeout = Duration::from_secs(self.connect_timeout_secs.max(1));
        Ok(config)
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `~/x` → `$HOME/x`.
fn expand(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => path.to_owned(),
    }
}

/// The host a job runs on, saved with it as `host.json` so `attach`
/// reaches the same machine even if `jefe.toml` changed since.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobHost {
    pub name: HostId,
    /// `None` for this machine.
    pub ssh: Option<HostConfig>,
}

impl JobHost {
    pub fn local() -> Self {
        Self {
            name: HostId::local(),
            ssh: None,
        }
    }

    pub fn is_local(&self) -> bool {
        self.ssh.is_none()
    }

    /// A connection to it. SSH hosts dial lazily; [`Host::connect`] surfaces
    /// auth and host key errors early.
    pub fn open(&self) -> anyhow::Result<Arc<Host>> {
        match &self.ssh {
            None => Ok(Arc::new(Host::local())),
            Some(c) => {
                let config = c.ssh_config().with_context(|| format!("host {}", self.name))?;
                Ok(Arc::new(Host::new(config)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn parse(text: &str) -> HostConfig {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn defaults_are_port_22_the_agent_and_known_hosts() {
        let c = parse("host = \"box\"\nuser = \"me\"\n");
        let s = c.ssh_config().unwrap();
        assert_eq!((s.host.as_str(), s.port, s.user.as_str()), ("box", 22, "me"));
        assert!(matches!(s.auth, Auth::Agent(AgentSocket::Env)));
        assert!(matches!(s.host_key, HostKeyCheck::KnownHosts(p) if p.ends_with(".ssh/known_hosts")));
    }

    #[test]
    fn fingerprints_replace_known_hosts() {
        let c = parse("host = \"box\"\nuser = \"me\"\nport = 2222\nfingerprints = [\"SHA256:x\"]\n");
        let s = c.ssh_config().unwrap();
        assert_eq!(s.port, 2222);
        assert!(matches!(s.host_key, HostKeyCheck::Fingerprints(f) if f == ["SHA256:x"]));
        let both = parse("host = \"box\"\nfingerprints = [\"SHA256:x\"]\nknown_hosts = \"/k\"\n");
        assert!(both.check().is_err());
    }

    #[test]
    fn a_missing_identity_is_an_error() {
        let c = parse("host = \"box\"\nuser = \"me\"\nidentity = \"/nonexistent/key\"\n");
        assert!(c.ssh_config().is_err());
    }
}
