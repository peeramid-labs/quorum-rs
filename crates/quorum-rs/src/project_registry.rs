//! Project registry — maps a project id (the epic's root-commit key from
//! patch-deliberation) to the agents that currently hold it.
//!
//! Agent checkouts live at different paths on different hosts, so the fleet cannot
//! find "an agent on this epic" by path. Agents advertise `{project_id, epic_head}`;
//! this registry groups them by the path-independent project id, so a read request
//! (or any discovery) can route to any live holder of the project. Fed by
//! advertisements over NATS; the map + staleness logic here are pure and testable
//! (`now` is injected, never read from the clock).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The wire message an agent publishes to say "I hold this epic." Keyed by the
/// project id; `epic_head` is the agent's current consensus sha (state, for
/// point-in-time routing); `host` disambiguates co-located checkouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAdvertisement {
    /// Path-independent project key (epic root-commit) — see `patch_deliberation::project`.
    pub project_id: String,
    /// The advertising agent's name/id.
    pub agent: String,
    /// The agent's current epic HEAD (consensus sha), if known.
    #[serde(default)]
    pub epic_head: Option<String>,
    /// The agent's host, to distinguish co-located checkouts of the same project.
    #[serde(default)]
    pub host: Option<String>,
}

impl ProjectAdvertisement {
    /// Build an advertisement from a `before_prompt` verdict `content` object — the
    /// dylib surfaces `project_id` (+ optional `epic_head`) onto it. `None` when there
    /// is no `project_id` (a non-patch-deliberation turn advertises nothing).
    pub fn from_verdict(
        content: &serde_json::Value,
        agent: &str,
        host: Option<String>,
    ) -> Option<Self> {
        let project_id = content.get("project_id")?.as_str()?.to_string();
        let epic_head = content
            .get("epic_head")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Some(Self {
            project_id,
            agent: agent.to_string(),
            epic_head,
            host,
        })
    }
}

/// A registered holder of a project, with the last time it was seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAgent {
    pub agent: String,
    pub epic_head: Option<String>,
    pub host: Option<String>,
    pub last_seen: DateTime<Utc>,
}

/// `project_id` → (`agent` → record). One entry per (project, agent); a re-advertise
/// refreshes it in place rather than duplicating.
#[derive(Debug, Default)]
pub struct ProjectRegistry {
    by_project: HashMap<String, HashMap<String, ProjectAgent>>,
}

impl ProjectRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or refresh) an advertisement as seen at `now`.
    pub fn record(&mut self, adv: &ProjectAdvertisement, now: DateTime<Utc>) {
        self.by_project
            .entry(adv.project_id.clone())
            .or_default()
            .insert(
                adv.agent.clone(),
                ProjectAgent {
                    agent: adv.agent.clone(),
                    epic_head: adv.epic_head.clone(),
                    host: adv.host.clone(),
                    last_seen: now,
                },
            );
    }

    /// Live holders of `project_id` (unordered; caller sorts if needed).
    pub fn agents_for(&self, project_id: &str) -> Vec<ProjectAgent> {
        self.by_project
            .get(project_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Every known project id.
    pub fn projects(&self) -> Vec<String> {
        self.by_project.keys().cloned().collect()
    }

    /// Drop agents not seen within `ttl` before `now`, then drop projects left empty —
    /// so a crashed/departed node stops being a routing target.
    pub fn prune(&mut self, now: DateTime<Utc>, ttl: Duration) {
        for agents in self.by_project.values_mut() {
            agents.retain(|_, a| now.signed_duration_since(a.last_seen) <= ttl);
        }
        self.by_project.retain(|_, m| !m.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(project: &str, agent: &str, head: Option<&str>) -> ProjectAdvertisement {
        ProjectAdvertisement {
            project_id: project.to_string(),
            agent: agent.to_string(),
            epic_head: head.map(str::to_string),
            host: Some("hostA".to_string()),
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn groups_agents_by_project_and_separates_distinct_projects() {
        let mut reg = ProjectRegistry::new();
        reg.record(&adv("root-epic", "A", Some("h1")), at(0));
        reg.record(&adv("root-epic", "B", Some("h1")), at(1));
        reg.record(&adv("other-epic", "C", None), at(2));

        let mut epic: Vec<_> = reg
            .agents_for("root-epic")
            .into_iter()
            .map(|a| a.agent)
            .collect();
        epic.sort();
        assert_eq!(
            epic,
            vec!["A", "B"],
            "both agents grouped under the shared epic"
        );
        assert_eq!(
            reg.agents_for("other-epic")
                .into_iter()
                .map(|a| a.agent)
                .collect::<Vec<_>>(),
            vec!["C"],
            "distinct project is separate"
        );
        assert!(
            reg.agents_for("unknown").is_empty(),
            "unknown project → no agents"
        );
        let mut projs = reg.projects();
        projs.sort();
        assert_eq!(projs, vec!["other-epic", "root-epic"]);
    }

    #[test]
    fn re_advertise_refreshes_in_place_and_updates_head() {
        let mut reg = ProjectRegistry::new();
        reg.record(&adv("epic", "A", Some("h1")), at(0));
        reg.record(&adv("epic", "A", Some("h2")), at(10));
        let agents = reg.agents_for("epic");
        assert_eq!(agents.len(), 1, "same agent is not duplicated");
        assert_eq!(agents[0].epic_head.as_deref(), Some("h2"), "head refreshed");
        assert_eq!(agents[0].last_seen, at(10), "last_seen refreshed");
    }

    #[test]
    fn prune_drops_stale_agents_and_empty_projects() {
        let mut reg = ProjectRegistry::new();
        reg.record(&adv("epic", "stale", Some("h1")), at(0));
        reg.record(&adv("epic", "fresh", Some("h1")), at(100));
        reg.record(&adv("gone", "only", None), at(0));

        // ttl = 60s, now = at(120): 'stale' (seen at 0, age 120) + 'only' (age 120) drop.
        reg.prune(at(120), Duration::seconds(60));

        let live: Vec<_> = reg
            .agents_for("epic")
            .into_iter()
            .map(|a| a.agent)
            .collect();
        assert_eq!(live, vec!["fresh"], "only the fresh agent survives");
        assert!(reg.agents_for("gone").is_empty(), "empty project pruned");
        assert_eq!(
            reg.projects(),
            vec!["epic"],
            "emptied project removed entirely"
        );
    }

    #[test]
    fn advertisement_from_verdict_extracts_project_and_head() {
        // Full: project_id + epic_head surfaced by the dylib.
        let content = serde_json::json!({
            "task_description": "…", "project_id": "root-sha", "epic_head": "head-sha"
        });
        let a = ProjectAdvertisement::from_verdict(&content, "AgentA", Some("h1".into())).unwrap();
        assert_eq!(a.project_id, "root-sha");
        assert_eq!(a.agent, "AgentA");
        assert_eq!(a.epic_head.as_deref(), Some("head-sha"));
        assert_eq!(a.host.as_deref(), Some("h1"));
        // project_id but no epic_head → still advertises, head None.
        let no_head = serde_json::json!({"project_id": "root-sha"});
        assert_eq!(
            ProjectAdvertisement::from_verdict(&no_head, "A", None)
                .unwrap()
                .epic_head,
            None
        );
        // No project_id (non-patch-deliberation turn) → nothing to advertise.
        let none = serde_json::json!({"task_description": "just a prompt"});
        assert!(ProjectAdvertisement::from_verdict(&none, "A", None).is_none());
    }

    #[test]
    fn advertisement_round_trips_json() {
        let a = adv("root-epic", "A", Some("h1"));
        let json = serde_json::to_string(&a).unwrap();
        let back: ProjectAdvertisement = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
        // Missing optional fields default to None.
        let minimal: ProjectAdvertisement =
            serde_json::from_str(r#"{"project_id":"p","agent":"A"}"#).unwrap();
        assert_eq!(minimal.epic_head, None);
        assert_eq!(minimal.host, None);
    }
}
