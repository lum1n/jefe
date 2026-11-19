//! A team preset: who orchestrates, who works, in which roles, within what
//! budget. One JSON shape (camelCase) shared by the CLI's config, the
//! desktop's settings and the iOS app's, so a preset made in one is usable
//! in the others. [`TeamPreset::apply`] turns it into a [`JobSpec`]'s
//! settings.

use std::collections::BTreeMap;

use acc_core::Harness;
use serde::{Deserialize, Serialize};

use crate::job::{Access, BrainKind, JobSpec, ReviewBy, RoleConfig};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TeamPreset {
    pub name: String,
    pub orchestrator: OrchestratorPreset,
    /// The defaults every role inherits.
    pub workers: AgentPreset,
    /// Per-role overrides, by role id. Roles not listed keep the built-in
    /// settings over `workers`.
    pub roles: BTreeMap<String, RolePreset>,
    pub budget: BudgetPreset,
    /// `None`: when attended.
    pub approve_plan: Option<bool>,
    /// Review the result before the job ends. `None`: when attended.
    pub approve_finish: Option<bool>,
    pub review_by: ReviewBy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OrchestratorPreset {
    pub kind: BrainKind,
    /// `None`: the workers' harness.
    pub harness: Option<Harness>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_turns: u32,
}

impl Default for OrchestratorPreset {
    fn default() -> Self {
        let b = crate::job::BrainSpec::default();
        Self {
            kind: BrainKind::Session,
            harness: b.harness,
            model: b.model,
            effort: b.effort,
            max_turns: b.max_turns,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentPreset {
    pub harness: Option<Harness>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RolePreset {
    pub enabled: bool,
    pub harness: Option<Harness>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// `None`: the role's built-in access.
    pub access: Option<Access>,
    pub max: Option<u32>,
}

impl Default for RolePreset {
    fn default() -> Self {
        Self {
            enabled: true,
            harness: None,
            model: None,
            effort: None,
            access: None,
            max: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BudgetPreset {
    pub max_workers: u32,
    pub max_tasks: u32,
    pub wall_minutes: u64,
    pub max_cost_usd: Option<f64>,
}

impl Default for BudgetPreset {
    fn default() -> Self {
        let b = crate::job::Budget::default();
        Self {
            max_workers: b.max_workers,
            max_tasks: b.max_tasks,
            wall_minutes: b.wall_ms / 60_000,
            max_cost_usd: b.max_cost_usd,
        }
    }
}

impl Default for TeamPreset {
    /// Nothing chosen: a session orchestrator on the workers' harness,
    /// the worker defaults, the built-in roles and budget.
    fn default() -> Self {
        Self {
            name: String::new(),
            orchestrator: OrchestratorPreset::default(),
            workers: AgentPreset::default(),
            roles: BTreeMap::new(),
            budget: BudgetPreset::default(),
            approve_plan: None,
            approve_finish: None,
            review_by: ReviewBy::default(),
        }
    }
}

impl TeamPreset {
    /// The preset every app ships: a Claude Opus orchestrator over pi
    /// workers on a cheap model, read-only roles with low effort.
    pub fn builtin() -> Self {
        let cheap = |effort: &str| RolePreset {
            effort: Some(effort.to_owned()),
            ..RolePreset::default()
        };
        Self {
            name: "Opus chief + pi/deepseek workers".to_owned(),
            orchestrator: OrchestratorPreset {
                kind: BrainKind::Session,
                harness: Some(Harness::Claude),
                model: Some("opus".to_owned()),
                effort: Some("high".to_owned()),
                max_turns: 40,
            },
            workers: AgentPreset {
                harness: Some(Harness::Pi),
                model: Some("opencode-go/deepseek-v4.1-flash".to_owned()),
                effort: None,
                permission: None,
            },
            roles: BTreeMap::from([
                ("explorer".to_owned(), cheap("low")),
                ("researcher".to_owned(), cheap("medium")),
                ("reviewer".to_owned(), cheap("medium")),
            ]),
            budget: BudgetPreset::default(),
            approve_plan: None,
            approve_finish: None,
            review_by: ReviewBy::Both,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Writes the preset into `spec`: orchestrator, worker defaults, roles,
    /// budget, approval and review mode. The job's goal, repository, host,
    /// prompts and the rest stay as they are.
    pub fn apply(&self, spec: &mut JobSpec) {
        spec.brain.kind = self.orchestrator.kind;
        spec.brain.harness = self.orchestrator.harness;
        spec.brain.model = self.orchestrator.model.clone();
        spec.brain.effort = self.orchestrator.effort.clone();
        spec.brain.max_turns = self.orchestrator.max_turns.max(1);
        if let Some(h) = self.workers.harness {
            spec.workers.harness = h;
            spec.workers.alternates.retain(|a| *a != h);
        }
        spec.workers.model = self.workers.model.clone();
        spec.workers.effort = self.workers.effort.clone();
        if let Some(p) = &self.workers.permission {
            spec.workers.permission = Some(p.clone());
        }
        for (id, r) in &self.roles {
            let mut role = spec.role(id).unwrap_or_default();
            role.enabled = r.enabled;
            role.harness = r.harness;
            role.model = r.model.clone();
            role.effort = r.effort.clone();
            if let Some(a) = r.access {
                role.access = a;
            }
            role.max = r.max;
            spec.roles.insert(id.clone(), role);
        }
        spec.budget.max_workers = self.budget.max_workers.max(1);
        spec.budget.max_tasks = self.budget.max_tasks.max(1);
        spec.budget.wall_ms = self.budget.wall_minutes.saturating_mul(60_000);
        spec.budget.max_cost_usd = self.budget.max_cost_usd;
        spec.approve_plan = self.approve_plan;
        spec.approve_finish = self.approve_finish;
        spec.review_by = self.review_by;
    }

    /// Why the preset can't run, if it can't.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("the preset needs a name".to_owned());
        }
        if self.budget.max_workers == 0 || self.budget.max_tasks == 0 {
            return Err("allow at least one worker and one task".to_owned());
        }
        let builtin = crate::job::builtin_roles();
        for id in self.roles.keys() {
            if !builtin.contains_key(id) && !crate::ids::valid_job_id(id) {
                return Err(format!("role id {id:?}: use letters, digits, '-' or '_'"));
            }
        }
        Ok(())
    }
}

/// Roles as a UI lists them: built-in order first.
pub fn role_order() -> [&'static str; 5] {
    ["explorer", "researcher", "implementor", "tester", "reviewer"]
}

/// The built-in roles with their descriptions, for a UI's role table.
pub fn builtin_role_configs() -> Vec<(String, RoleConfig)> {
    let roles = crate::job::builtin_roles();
    role_order()
        .iter()
        .filter_map(|id| roles.get(*id).map(|r| ((*id).to_owned(), r.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_builtin_preset_round_trips_and_applies() {
        let p = TeamPreset::builtin();
        let json = p.to_json();
        assert!(json.contains("\"maxWorkers\""), "{json}");
        assert_eq!(TeamPreset::from_json(&json).unwrap(), p);
        let mut spec = JobSpec::new("j", "g", "/r");
        p.apply(&mut spec);
        assert_eq!(spec.brain.kind, BrainKind::Session);
        assert_eq!(spec.brain.harness, Some(Harness::Claude));
        assert_eq!(spec.workers.harness, Harness::Pi);
        assert!(!spec.workers.alternates.contains(&Harness::Pi));
        let explorer = spec.resolve("explorer").unwrap();
        assert_eq!(explorer.effort.as_deref(), Some("low"));
        assert_eq!(explorer.model.as_deref(), Some("opencode-go/deepseek-v4.1-flash"));
        assert_eq!(spec.resolve("implementor").unwrap().harness, Harness::Pi);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn a_sparse_json_fills_in_defaults() {
        let p = TeamPreset::from_json(r#"{"name":"x","roles":{"tester":{"enabled":false}}}"#).unwrap();
        assert_eq!(p.budget.max_workers, 3);
        assert!(!p.roles["tester"].enabled);
        let mut spec = JobSpec::new("j", "g", "/r");
        p.apply(&mut spec);
        assert!(spec.resolve("tester").is_none());
        assert_eq!(spec.workers.harness, Harness::Claude);
    }
}
