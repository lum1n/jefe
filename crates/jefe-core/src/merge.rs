//! Merging accepted work into the base branch, and what to do when it conflicts.

use acc_core::shell::quote;

use crate::ids::{JobId, TaskId, WorkerId};

/// The commit message for a task's merge.
pub fn message(title: &str, job: &JobId, task: &TaskId, worker: &WorkerId) -> String {
    format!("{title}\n\njefe {job} {task} by {worker}")
}

/// Checks the repository before any worker starts: prints its top level,
/// the branch checked out, then any uncommitted changes to tracked files.
pub fn preflight_script(repo: &str) -> String {
    format!(
        "cd {repo} 2>/dev/null || {{ echo 'jefe: no such directory' >&2; exit 6; }}\n\
         git rev-parse --show-toplevel || exit 6\n\
         git symbolic-ref -q --short HEAD || echo\n\
         git status --porcelain --untracked-files=no\n",
        repo = quote(repo),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    pub top: String,
    /// Empty on a detached HEAD.
    pub branch: String,
    pub dirty: bool,
}

pub fn parse_preflight(stdout: &str) -> Option<Preflight> {
    let mut lines = stdout.lines();
    let top = lines.next()?.trim().to_owned();
    if top.is_empty() {
        return None;
    }
    let branch = lines.next().unwrap_or_default().trim().to_owned();
    let dirty = lines.any(|l| !l.trim().is_empty());
    Some(Preflight { top, branch, dirty })
}

/// Lists the files `branch` conflicts on when merged into the checked-out
/// branch, leaving the repository as it was.
pub fn conflict_files_script(repo: &str, branch: &str) -> String {
    format!(
        "cd {repo} || exit 6\n\
         git -c user.name=jefe -c user.email=jefe@localhost merge --no-commit --no-ff {branch} >/dev/null 2>&1\n\
         git diff --name-only --diff-filter=U\n\
         git merge --abort 2>/dev/null || git reset -q --merge\n\
         true\n",
        repo = quote(repo),
        branch = quote(branch),
    )
}

/// The brief of a task that resolves `branch`'s conflict.
pub fn conflict_brief(title: &str, branch: &str, files: &[String]) -> String {
    let mut brief = format!(
        "The work for \"{title}\" is on branch `{branch}`, but merging it conflicts with what landed since. \
         Run `git merge {branch}` in your worktree, resolve every conflict so both sides' intent is kept, \
         run the tests, and commit the merge."
    );
    if !files.is_empty() {
        brief.push_str("\n\nConflicting files:\n");
        for f in files {
            brief.push_str(&format!("- {f}\n"));
        }
    }
    brief
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_preflight() {
        let p = parse_preflight("/r\nmain\n M a.rs\n").unwrap();
        assert_eq!(p.branch, "main");
        assert!(p.dirty);
        assert!(!parse_preflight("/r\n\n").unwrap().dirty);
        assert_eq!(parse_preflight(""), None);
    }
}
