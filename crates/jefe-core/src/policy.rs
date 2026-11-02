//! The approvals a worker gets without asking, and the brain's read-only
//! rules. jefe is the one client holding policy for the sessions it starts.

use acc_core::policy::{Decision, Rule};

/// Commands a worker may run without asking (whole words; acc never lets an
/// allow rule match a chained command).
const WORKER_ALLOW: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "grep",
    "rg",
    "find",
    "tree",
    "pwd",
    "which",
    "diff",
    "file",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git blame",
    "git grep",
    "git ls-files",
    "git add",
    "git commit",
    "git merge",
    "git checkout",
    "git switch",
    "git restore",
    "git stash",
    "git rm",
    "git mv",
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo fmt",
    "cargo nextest",
    "go build",
    "go test",
    "go vet",
    "gofmt",
    "golangci-lint",
    "npm test",
    "npm run",
    "pnpm test",
    "pnpm run",
    "pnpm exec",
    "pnpm lint",
    "yarn test",
    "yarn run",
    "bun test",
    "bun run",
    "npx tsc",
    "npx eslint",
    "npx prettier",
    "npx vitest",
    "npx jest",
    "pytest",
    "python -m pytest",
    "python3 -m pytest",
    "python -m unittest",
    "python3 -m unittest",
    "ruff",
    "mypy",
    "swift build",
    "swift test",
    "mvn test",
    "./gradlew test",
    "make",
    "just",
];

/// Never for a worker: publishing and repository configuration stay with the
/// human.
const WORKER_DENY: &[&str] = &["git push", "gh", "git remote", "git config", "git worktree"];

/// Commands the brain may run while it looks around.
const BRAIN_ALLOW: &[&str] = &[
    "git diff",
    "git log",
    "git show",
    "git status",
    "cat",
    "ls",
    "rg",
    "grep",
    "head",
    "tail",
];

fn shell(decision: Decision, command: &str) -> Rule {
    Rule {
        decision,
        tool: Some("shell".to_owned()),
        path: None,
        command: Some(command.to_owned()),
    }
}

fn tool(decision: Decision, tool: &str) -> Rule {
    Rule {
        decision,
        tool: Some(tool.to_owned()),
        path: None,
        command: None,
    }
}

/// A worker's rules: the defaults, then `extra` from the config. Deny wins
/// over allow in acc, so `extra` can't lift the deny list.
pub fn worker_rules(extra: &[Rule]) -> Vec<Rule> {
    let mut out: Vec<Rule> = WORKER_DENY.iter().map(|c| shell(Decision::Deny, c)).collect();
    out.push(tool(Decision::Allow, "read"));
    out.push(tool(Decision::Allow, "edit"));
    out.extend(WORKER_ALLOW.iter().map(|c| shell(Decision::Allow, c)));
    out.extend(extra.iter().cloned());
    out
}

/// The brain's rules: reading and a few read-only commands; edits denied.
/// Anything else no rule matches is rejected by the host.
pub fn brain_rules() -> Vec<Rule> {
    let mut out = vec![tool(Decision::Deny, "edit"), tool(Decision::Allow, "read")];
    out.extend(BRAIN_ALLOW.iter().map(|c| shell(Decision::Allow, c)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use acc_core::policy::{Policy, Request, ToolKind};

    fn decide(rules: Vec<Rule>, command: &str) -> Option<Decision> {
        let req = Request {
            tool: Some("Bash".to_owned()),
            kind: Some(ToolKind::Shell),
            command: Some(command.to_owned()),
            paths: Vec::new(),
        };
        Policy { rules }.decide(&req, "/w").map(|(d, _)| d)
    }

    #[test]
    fn workers_test_but_never_push() {
        assert_eq!(
            decide(worker_rules(&[]), "cargo test --all"),
            Some(Decision::Allow)
        );
        assert_eq!(
            decide(worker_rules(&[]), "git push origin main"),
            Some(Decision::Deny)
        );
        let extra = [shell(Decision::Allow, "git push")];
        assert_eq!(decide(worker_rules(&extra), "git push"), Some(Decision::Deny));
        assert_eq!(decide(worker_rules(&[]), "curl x"), None);
    }

    #[test]
    fn the_brain_only_reads() {
        assert_eq!(decide(brain_rules(), "git log -3"), Some(Decision::Allow));
        assert_eq!(decide(brain_rules(), "cargo test"), None);
    }
}
