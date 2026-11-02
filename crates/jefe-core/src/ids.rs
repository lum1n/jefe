//! Identifiers. Worker ids are acc session ids, so they follow acc's rules
//! (`[A-Za-z0-9_-]{1,64}`).

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

id!(
    /// A job: one goal in one repository.
    JobId
);
id!(
    /// A worker: one acc session, and the worktree it was first launched in.
    WorkerId
);
id!(
    /// A task within a job (`t1`, `t2`, ...).
    TaskId
);
id!(
    /// A question to the brain or the human (`a1`, `a2`, ...).
    AskId
);
id!(
    /// Where workers run: `local` or a configured SSH host.
    HostId
);

impl HostId {
    pub fn local() -> Self {
        Self("local".to_owned())
    }
}

/// An exec the host runs for the loop; its result comes back with the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecId(pub u64);

impl fmt::Display for ExecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "x{}", self.0)
    }
}

/// Whether `id` can name a job: short enough that `jefe-<job>-w<n>` stays a
/// valid acc session id.
pub fn valid_job_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A worker's id: `jefe-<job>-w<n>`.
pub fn worker_id(job: &JobId, n: u32) -> WorkerId {
    WorkerId(format!("jefe-{job}-w{n}"))
}

/// A brain session's id: `jefe-<job>-b<n>`.
pub fn brain_id(job: &JobId, n: u32) -> String {
    format!("jefe-{job}-b{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_ids_are_valid_acc_names() {
        let job = JobId("a".repeat(40));
        assert!(valid_job_id(job.as_str()));
        assert!(acc_core::worktree::valid_name(worker_id(&job, 999).as_str()));
        assert!(!valid_job_id("has space"));
    }
}
