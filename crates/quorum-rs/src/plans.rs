//! What an operator's subscription entitles them to.
//!
//! Running an agent is not a paid feature. Anyone with an account may register
//! one and use it themselves, at a bounded cost because the limits below apply
//! to everybody. What a subscription buys is *reach*: an unsubscribed
//! operator's agents are theirs alone, a subscriber's appear in the public
//! directory where other people can hire them.
//!
//! The noolog node records each operator's plan and holds the [`PlanConfig`];
//! everything else asks the node for a [`PlanRecord`]. The types live here so
//! the node and its readers share one definition.

use serde::{Deserialize, Serialize};

/// Which plan an operator is on.
///
/// Ordered by what it grants, so a comparison reads the way the pricing page
/// does. `Free` is every account that has never paid, and what an operator
/// falls back to when a subscription lapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OperatorPlan {
    #[default]
    Free,
    Basic,
    Pro,
}

impl OperatorPlan {
    /// The stored form, and what an API answers with.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Basic => "basic",
            Self::Pro => "pro",
        }
    }

    /// A plan somebody asked for by name; `None` for a name that is not a plan,
    /// which a caller reports rather than reading as `free`.
    pub fn named(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "free" => Some(Self::Free),
            "basic" => Some(Self::Basic),
            "pro" => Some(Self::Pro),
            _ => None,
        }
    }

    /// A stored plan read back, falling back to [`Self::Free`].
    ///
    /// An unknown value is the free plan rather than an error: the alternative
    /// is an account that can do nothing because its record was written by a
    /// newer build than the one reading it.
    pub fn from_str_or_free(raw: &str) -> Self {
        Self::named(raw).unwrap_or_default()
    }
}

/// What one plan allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLimits {
    /// How many agents the operator may have registered at once.
    pub agents: usize,
    /// The operator's file space, in bytes.
    pub storage_bytes: i64,
    /// Whether this operator's agents appear in the public directory: the one
    /// thing a subscription buys. Without it an operator still runs agents;
    /// only they can see or hire them.
    pub listed: bool,
    /// How many list names the operator may hold at once. A name is a
    /// subdomain anyone can type, so the cap is what stops one account taking
    /// all the good ones.
    #[serde(default)]
    pub lists: usize,
}

impl PlanLimits {
    /// The limits for `plan`, as configured.
    pub fn for_plan(config: &PlanConfig, plan: OperatorPlan) -> Self {
        match plan {
            OperatorPlan::Free => config.free,
            OperatorPlan::Basic => config.basic,
            OperatorPlan::Pro => config.pro,
        }
    }
}

/// The three plans a deployment offers. The numbers are a commercial decision
/// that changes without the code changing, so they are configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanConfig {
    #[serde(default = "default_free")]
    pub free: PlanLimits,
    #[serde(default = "default_basic")]
    pub basic: PlanLimits,
    #[serde(default = "default_pro")]
    pub pro: PlanLimits,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            free: default_free(),
            basic: default_basic(),
            pro: default_pro(),
        }
    }
}

/// One agent and a small space, private to its owner: the product is worth
/// nothing to somebody who cannot try it.
fn default_free() -> PlanLimits {
    PlanLimits {
        agents: 1,
        storage_bytes: 100 * 1024 * 1024,
        listed: false,
        lists: 0,
    }
}

/// The same allowance, listed publicly.
fn default_basic() -> PlanLimits {
    PlanLimits {
        agents: 1,
        storage_bytes: 100 * 1024 * 1024,
        listed: true,
        lists: 1,
    }
}

fn default_pro() -> PlanLimits {
    PlanLimits {
        agents: 10,
        storage_bytes: 20 * 1024 * 1024 * 1024,
        listed: true,
        lists: 5,
    }
}

/// An operator's plan and what it allows, as the node answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRecord {
    pub plan: OperatorPlan,
    pub limits: PlanLimits,
}

impl PlanRecord {
    pub fn new(config: &PlanConfig, plan: OperatorPlan) -> Self {
        Self {
            plan,
            limits: PlanLimits::for_plan(config, plan),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_or_missing_plan_is_the_free_one() {
        // Reading an unrecognised value as the largest plan would give away
        // what the deployment meant to sell, silently; reading it as the
        // smallest produces a complaint from somebody who paid.
        assert_eq!(OperatorPlan::from_str_or_free("pro"), OperatorPlan::Pro);
        assert_eq!(OperatorPlan::from_str_or_free("BASIC"), OperatorPlan::Basic);
        assert_eq!(
            OperatorPlan::from_str_or_free("  free "),
            OperatorPlan::Free
        );
        assert_eq!(OperatorPlan::from_str_or_free(""), OperatorPlan::Free);
        assert_eq!(
            OperatorPlan::from_str_or_free("enterprise"),
            OperatorPlan::Free
        );
        assert_eq!(OperatorPlan::default(), OperatorPlan::Free);
    }

    #[test]
    fn a_plan_asked_for_by_a_name_that_is_no_plan_is_refused() {
        assert_eq!(OperatorPlan::named(" Pro "), Some(OperatorPlan::Pro));
        assert_eq!(OperatorPlan::named("enterprise"), None);
        assert_eq!(OperatorPlan::named(""), None);
    }

    #[test]
    fn a_plan_survives_the_form_it_is_stored_in() {
        for plan in [OperatorPlan::Free, OperatorPlan::Basic, OperatorPlan::Pro] {
            assert_eq!(OperatorPlan::from_str_or_free(plan.as_str()), plan);
            let json = serde_json::to_string(&plan).unwrap();
            assert_eq!(json, format!("\"{}\"", plan.as_str()));
        }
    }

    #[test]
    fn running_an_agent_needs_no_subscription_but_being_listed_does() {
        // The commercial shape in one test. If the free plan stops allowing an
        // agent the product stops being tryable; if it starts being listed the
        // subscription stops being worth buying.
        let config = PlanConfig::default();

        let free = PlanLimits::for_plan(&config, OperatorPlan::Free);
        assert!(free.agents >= 1);
        assert!(!free.listed);
        assert_eq!(free.lists, 0);

        let basic = PlanLimits::for_plan(&config, OperatorPlan::Basic);
        assert!(basic.listed);

        let pro = PlanLimits::for_plan(&config, OperatorPlan::Pro);
        assert!(pro.listed);
        assert!(pro.agents > basic.agents);
        assert!(pro.storage_bytes > basic.storage_bytes);
        assert_eq!(pro.storage_bytes, 20 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_config_written_before_lists_existed_sells_no_names() {
        let config: PlanConfig =
            serde_json::from_str(r#"{"pro": {"agents": 3, "storage_bytes": 10, "listed": true}}"#)
                .unwrap();
        assert_eq!(config.pro.lists, 0);
        assert_eq!(config.pro.agents, 3);
        assert_eq!(config.free, PlanConfig::default().free);
    }

    #[test]
    fn a_record_carries_the_limits_of_its_plan() {
        let config = PlanConfig::default();
        let record = PlanRecord::new(&config, OperatorPlan::Basic);
        assert_eq!(record.limits, config.basic);
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["plan"], "basic");
        assert_eq!(json["limits"]["lists"], 1);
        assert_eq!(serde_json::from_value::<PlanRecord>(json).unwrap(), record);
    }
}
