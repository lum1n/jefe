//! A job's directory: `job.json` (the latest snapshot), `events.ndjson`,
//! `inputs.ndjson` (everything fed to the loop, for replay), `report.md`,
//! `brain.json`, `host.json`, `workers/<id>/final.md`, and `pid` while a jefe process drives it.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use jefe_core::{Input, Report, Stamped};
use serde::Serialize;

#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    events: File,
    inputs: File,
}

/// One line of `inputs.ndjson`.
#[derive(Serialize)]
struct Fed<'a> {
    at_ms: u64,
    #[serde(flatten)]
    input: &'a Input,
}

impl Store {
    pub fn dir_for(id: &str) -> PathBuf {
        crate::paths::jobs_dir().join(id)
    }

    /// Opens (creating) the job's directory and claims it for this process.
    pub fn open(id: &str) -> anyhow::Result<Self> {
        let dir = Self::dir_for(id);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let append = |name: &str| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(name))
                .with_context(|| format!("opening {}", dir.join(name).display()))
        };
        let store = Self {
            events: append("events.ndjson")?,
            inputs: append("inputs.ndjson")?,
            dir,
        };
        write_atomic(&store.dir.join("pid"), std::process::id().to_string().as_bytes())?;
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn save(&self, snapshot: &str) -> anyhow::Result<()> {
        write_atomic(&self.dir.join("job.json"), snapshot.as_bytes())
    }

    pub fn event(&mut self, s: &Stamped) -> anyhow::Result<()> {
        line(&mut self.events, s)
    }

    pub fn input(&mut self, input: &Input, at_ms: u64) -> anyhow::Result<()> {
        line(&mut self.inputs, &Fed { at_ms, input })
    }

    pub fn report(&self, report: &Report) -> anyhow::Result<()> {
        write_atomic(
            &self.dir.join("report.md"),
            crate::output::report_md(report).as_bytes(),
        )
    }

    /// Which brain the job runs with, for `attach`.
    pub fn save_brain(&self, brain: Option<&crate::brain::HarnessBrain>) -> anyhow::Result<()> {
        write_atomic(
            &self.dir.join("brain.json"),
            serde_json::to_string(&brain)?.as_bytes(),
        )
    }

    /// A worker's last message when it reports, as `workers/<id>/final.md`.
    pub fn worker_final(&self, worker: &str, text: &str) -> anyhow::Result<()> {
        let dir = self.dir.join("workers").join(worker);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        write_atomic(&dir.join("final.md"), text.as_bytes())
    }

    /// Which host the job runs on, for `attach`.
    pub fn save_host(&self, host: &crate::hosts::JobHost) -> anyhow::Result<()> {
        write_atomic(
            &self.dir.join("host.json"),
            serde_json::to_string(host)?.as_bytes(),
        )
    }

    /// Gives the job up: another process may drive it now.
    pub fn release(&self) {
        let _ = fs::remove_file(self.dir.join("pid"));
    }
}

fn line<T: Serialize>(f: &mut File, value: &T) -> anyhow::Result<()> {
    let mut text = serde_json::to_string(value)?;
    text.push('\n');
    f.write_all(text.as_bytes())?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The latest snapshot of job `id`.
pub fn load(id: &str) -> anyhow::Result<String> {
    let path = Store::dir_for(id).join("job.json");
    fs::read_to_string(&path).with_context(|| format!("no job {id} ({})", path.display()))
}

/// The harness brain job `id` was started with; `None` for the rules.
pub fn load_brain(id: &str) -> anyhow::Result<Option<crate::brain::HarnessBrain>> {
    match fs::read_to_string(Store::dir_for(id).join("brain.json")) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The host job `id` runs on; this machine when it never saved one.
pub fn load_host(id: &str) -> anyhow::Result<crate::hosts::JobHost> {
    match fs::read_to_string(Store::dir_for(id).join("host.json")) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(crate::hosts::JobHost::local()),
        Err(e) => Err(e.into()),
    }
}

/// The pid of the jefe process driving job `id`, if one is alive.
pub fn driver_pid(id: &str) -> Option<u32> {
    let pid: u32 = fs::read_to_string(Store::dir_for(id).join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if pid == std::process::id() {
        return None;
    }
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    alive.then_some(pid)
}

/// Every job id, newest directory first.
pub fn job_ids() -> Vec<String> {
    let Ok(rd) = fs::read_dir(crate::paths::jobs_dir()) else {
        return Vec::new();
    };
    let mut jobs: Vec<(std::time::SystemTime, String)> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().join("job.json").is_file())
        .filter_map(|e| {
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, e.file_name().to_string_lossy().into_owned()))
        })
        .collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.0));
    jobs.into_iter().map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn input_lines_read_back_as_inputs() {
        for input in [
            Input::Tick,
            Input::Bytes {
                worker: jefe_core::WorkerId("w".to_owned()),
                bytes: b"{}\n".to_vec(),
            },
        ] {
            let line = serde_json::to_string(&Fed {
                at_ms: 7,
                input: &input,
            })
            .unwrap();
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["at_ms"], 7);
            assert_eq!(serde_json::from_value::<Input>(v).unwrap(), input);
        }
    }
}
