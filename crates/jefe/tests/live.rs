//! Against a signed-in harness: `JEFE_LIVE=1 cargo test -p jefe --test live -- --ignored`.
//! Uses the real `$HOME` (for the login) with a throwaway repository and state.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

fn live() -> bool {
    std::env::var_os("JEFE_LIVE").is_some()
}

#[test]
#[ignore = "needs a signed-in Claude Code and JEFE_LIVE=1"]
fn claude_brain_plans_and_a_claude_worker_delivers() {
    if !live() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let ok = Command::new("sh")
        .arg("-c")
        .arg("git init -q -b main && echo '# live' > README.md && git add README.md && git commit -qm init")
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(ok.success());
    std::fs::write(
        tmp.path().join("jefe.toml"),
        "[brain]\nkind = \"harness\"\nsandbox = false\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_jefe"))
        .args([
            "Add a file hello.txt containing the single line: hello",
            "--json",
            "--unattended",
            "--repo",
        ])
        .arg(&repo)
        .env("JEFE_CONFIG", tmp.path().join("jefe.toml"))
        .env("JEFE_STATE_DIR", tmp.path().join("state"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The timeline, for `--nocapture`: seconds since the first event, and how
    // long each brain consult and each worker turn took.
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|l| l["type"] == "event")
        .collect();
    let t0 = lines.first().and_then(|l| l["at_ms"].as_u64()).unwrap_or(0);
    let mut since: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut slowest = 0;
    for l in &lines {
        let (e, at) = (&l["event"], l["at_ms"].as_u64().unwrap_or(0));
        let kind = e["type"].as_str().unwrap_or("");
        let key = match kind {
            "consulted" | "decided" => format!("ask {}", e["ask"].as_str().unwrap_or("")),
            "worker_spawned" | "worker_prompted" | "worker_reported" => {
                format!("worker {}", e["worker"].as_str().unwrap_or(""))
            }
            _ => String::new(),
        };
        let took = match kind {
            "consulted" | "worker_spawned" | "worker_prompted" => {
                since.insert(key, at);
                String::new()
            }
            "decided" | "worker_reported" => since.remove(&key).map_or(String::new(), |s| {
                let ms = at.saturating_sub(s);
                slowest = slowest.max(ms);
                format!(" (took {:.1} s)", ms as f64 / 1000.0)
            }),
            _ => String::new(),
        };
        let what = e["decision"]["action"]
            .as_str()
            .or(e["message"].as_str())
            .or(e["title"].as_str())
            .unwrap_or("");
        eprintln!(
            "{:7.1} s  {kind} {what}{took}",
            at.saturating_sub(t0) as f64 / 1000.0
        );
    }
    let total = lines.last().and_then(|l| l["at_ms"].as_u64()).unwrap_or(t0) - t0;
    eprintln!(
        "total {:.1} s, slowest step {:.1} s",
        total as f64 / 1000.0,
        slowest as f64 / 1000.0
    );
    if total > 120_000 {
        // Keep a slow run's state (brain/<ask>.md, events) to see where the time went.
        eprintln!("slow run kept in {}", tmp.keep().display());
    }
    let decided: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|l| l["event"]["type"] == "decided")
        .collect();
    let plan = decided
        .iter()
        .find(|d| d["event"]["decision"]["action"] == "plan")
        .unwrap_or_else(|| panic!("no plan:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(plan["event"]["decision"]["source"]["by"], "brain", "{stdout}");
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let hello = std::fs::read_to_string(repo.join("hello.txt")).unwrap();
    assert_eq!(hello.trim(), "hello");
}
