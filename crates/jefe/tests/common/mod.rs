//! What the end-to-end tests share: a temporary `$HOME` with a fresh
//! repository and fake harnesses.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Speaks just enough of Claude Code's stream-json: on each prompt it waits
/// for `$HOME/go` (when `$HOME/hold` exists), commits a file and ends the
/// turn with a `jefe-result` block.
pub const FAKE_CLAUDE: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      touch "$HOME/started"
      if [ -e "$HOME/hold" ]; then
        while [ ! -e "$HOME/go" ]; do sleep 0.1; done
      fi
      printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-1","cwd":"/","model":"fake","tools":[],"permissionMode":"acceptEdits"}'
      branch=$(printf '%s' "$line" | grep -o 'Run `git merge acc/[A-Za-z0-9_-]*' | head -1 | sed 's/.*merge //')
      case "$line" in
        *'Run `git merge acc/'*)
          git merge -q "$branch" >/dev/null 2>&1
          printf 'a\nb\n' > hello.txt
          git add hello.txt && git commit -qm "Merge $branch keeping both" ;;
        *'Your task: Write A'*) echo a > hello.txt; git add hello.txt && git commit -qm "Write a" ;;
        *'Your task: Write B'*) echo b > hello.txt; git add hello.txt && git commit -qm "Write b" ;;
        *) echo hello > hello.txt; git add hello.txt && git commit -qm "Add hello.txt" ;;
      esac
      printf '%s\n' '{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"fake","content":[{"type":"text","text":"Added it.\n\n```jefe-result\n{\"status\":\"done\",\"summary\":\"Added hello.txt\"}\n```"}]},"session_id":"fake-1"}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Added it.","session_id":"fake-1","num_turns":1,"total_cost_usd":0,"usage":{"input_tokens":10,"output_tokens":5}}'
      ;;
  esac
done
"##;

/// A brain on Claude's stream-json: plans one task, accepts it and wraps
/// up, by the trigger named in its prompt. With `$HOME/brain-garbage` it
/// never gives a decision block.
pub const FAKE_BRAIN: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      printf '%s\n' "$line" >> "$HOME/brain-prompts"
      text='Looks fine to me.'
      if [ ! -e "$HOME/brain-garbage" ]; then
        task=$(printf '%s' "$line" | grep -o 'kind\\": \\"review\\",\\n *\\"task\\": \\"t[0-9]*' | head -1 | grep -o 't[0-9]*$')
        case "$line" in
          *'Why you are asked: plan'*'write two'*|*'write two'*'Why you are asked: plan'*)
            text='Two tasks.\n\n```jefe-decision\n{\"action\":\"plan\",\"tasks\":[{\"title\":\"Write A\",\"brief\":\"Write a to hello.txt.\"},{\"title\":\"Write B\",\"brief\":\"Write b to hello.txt.\"}],\"confidence\":0.9}\n```' ;;
          *'Why you are asked: plan'*)
            text='One task.\n\n```jefe-decision\n{\"action\":\"plan\",\"tasks\":[{\"title\":\"Add hello.txt\",\"brief\":\"Create hello.txt saying hello and commit it.\"}],\"rationale\":\"one file\",\"confidence\":0.9}\n```' ;;
          *'Why you are asked: review'*)
            text='Good.\n\n```jefe-decision\n{\"action\":\"accept\",\"task\":\"'"$task"'\",\"rationale\":\"matches the brief\",\"confidence\":0.8}\n```' ;;
          *'Why you are asked: wrap'*)
            text='```jefe-decision\n{\"action\":\"done\",\"summary\":\"hello.txt added\",\"confidence\":0.9}\n```' ;;
        esac
      fi
      printf '%s\n' '{"type":"system","subtype":"init","session_id":"brain-1","cwd":"/","model":"fake-brain","tools":[],"permissionMode":"default"}'
      printf '%s%s%s\n' '{"type":"assistant","message":{"id":"msg_b","type":"message","role":"assistant","model":"fake-brain","content":[{"type":"text","text":"' "$text" '"}]},"session_id":"brain-1"}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"brain-1","num_turns":1,"total_cost_usd":0,"usage":{"input_tokens":10,"output_tokens":5}}'
      ;;
  esac
done
"##;

pub fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub struct Env {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
}

impl Env {
    /// A temporary `$HOME` with a git identity, a repository on `main`, the
    /// fake harness and a config pointing at it.
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = t\n\temail = t@t\n[init]\n\tdefaultBranch = main\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        sh(
            &root.join("repo"),
            &format!(
                "export HOME={home}; git init -q -b main \
                 && echo '# proj' > README.md && git add README.md && git commit -qm init",
                home = home.display()
            ),
        );
        let fake = root.join("bin/claude");
        std::fs::create_dir_all(fake.parent().unwrap()).unwrap();
        std::fs::write(&fake, FAKE_CLAUDE).unwrap();
        let brain = root.join("bin/brain");
        std::fs::write(&brain, FAKE_BRAIN).unwrap();
        sh(&root, &format!("chmod +x {} {}", fake.display(), brain.display()));
        std::fs::write(
            root.join("jefe.toml"),
            format!(
                "[workers]\nharness = \"claude\"\nlauncher = {{ program = \"{}\" }}\n\n\
                 [brain]\nsandbox = false\ntimeout_secs = 30\nlauncher = {{ program = \"{}\" }}\n",
                fake.display(),
                brain.display()
            ),
        )
        .unwrap();
        Self { _tmp: tmp, root }
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    pub fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_jefe"));
        c.args(args)
            .current_dir(self.repo())
            .env("HOME", self.home())
            .env("JEFE_CONFIG", self.root.join("jefe.toml"))
            .env("JEFE_STATE_DIR", self.root.join("state"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::null());
        c
    }

    pub fn jefe(&self, args: &[&str]) -> Output {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(120) {
                let _ = child.kill();
                panic!("jefe {args:?} took too long");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        child.wait_with_output().unwrap()
    }

    pub fn acc_dir_is_empty(&self, sub: &str) -> bool {
        std::fs::read_dir(self.home().join(".acc").join(sub)).map_or(true, |mut d| d.next().is_none())
    }
}

pub fn events(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

pub fn event_types(lines: &[serde_json::Value]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l["type"] == "event")
        .map(|l| l["event"]["type"].as_str().unwrap().to_owned())
        .collect()
}

pub fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
