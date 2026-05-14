//! HITL control plane traits and types.
//!
//! Defines the [`AgentControlPlane`] trait for runtime agent control
//! (pause, config patching, buffer management) and the [`ConfigPatch`] struct
//! for partial live-updates to [`AgentConfig`](crate::AgentConfig) fields.
//!
//! Any agent implementation can satisfy [`AgentControlPlane`] to expose a
//! control plane; the reference implementation ships with the embedded
//! status server in this crate.

use crate::workers::buffer::BufferEntrySummary;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ConfigPatch
// ---------------------------------------------------------------------------

/// Subset of [`AgentConfig`](crate::AgentConfig) fields that can be live-patched
/// at runtime via the HITL control plane.
///
/// All fields are `Option` — only non-`None` values are applied.
/// Changes are **in-memory only** (lost on restart).
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    #[schema(minimum = 0.0, maximum = 2.0)]
    pub temperature: Option<f32>,
    #[schema(minimum = -2.0, maximum = 2.0)]
    pub frequency_penalty: Option<f32>,
    #[schema(minimum = -2.0, maximum = 2.0)]
    pub presence_penalty: Option<f32>,
    pub persona: Option<String>,
    pub textual_feedback: Option<bool>,
    #[schema(minimum = 1)]
    pub max_react_iterations: Option<i32>,
    #[schema(minimum = 0)]
    pub max_retries: Option<i32>,
}

impl ConfigPatch {
    /// Validate all patch values before applying.
    ///
    /// Returns an error message describing the first invalid field found.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(t) = self.temperature {
            if !t.is_finite() || !(0.0..=2.0).contains(&t) {
                return Err(format!(
                    "temperature must be finite and in [0.0, 2.0], got {}",
                    t
                ));
            }
        }
        if let Some(fp) = self.frequency_penalty {
            if !fp.is_finite() || !(-2.0..=2.0).contains(&fp) {
                return Err(format!(
                    "frequency_penalty must be finite and in [-2.0, 2.0], got {}",
                    fp
                ));
            }
        }
        if let Some(pp) = self.presence_penalty {
            if !pp.is_finite() || !(-2.0..=2.0).contains(&pp) {
                return Err(format!(
                    "presence_penalty must be finite and in [-2.0, 2.0], got {}",
                    pp
                ));
            }
        }
        if let Some(mri) = self.max_react_iterations {
            if mri < 1 {
                return Err(format!("max_react_iterations must be >= 1, got {}", mri));
            }
        }
        if let Some(mr) = self.max_retries {
            if mr < 0 {
                return Err(format!("max_retries must be >= 0, got {}", mr));
            }
        }
        Ok(())
    }

    /// Validate and apply this patch to an [`AgentConfig`](crate::AgentConfig).
    ///
    /// Returns `Err` if any patched value is out of range.
    pub fn apply(&self, config: &mut crate::AgentConfig) -> Result<(), String> {
        self.validate()?;
        if let Some(t) = self.temperature {
            config.temperature = t;
        }
        if let Some(fp) = self.frequency_penalty {
            config.frequency_penalty = Some(fp);
        }
        if let Some(pp) = self.presence_penalty {
            config.presence_penalty = Some(pp);
        }
        if let Some(ref p) = self.persona {
            config.persona = Some(p.clone());
        }
        if let Some(tf) = self.textual_feedback {
            config.textual_feedback = tf;
        }
        if let Some(mri) = self.max_react_iterations {
            config.max_react_iterations = Some(mri);
        }
        if let Some(mr) = self.max_retries {
            config.max_retries = Some(mr);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AgentControlPlane trait
// ---------------------------------------------------------------------------

/// Trait for controlling agents at runtime (pause, config, buffer ops).
///
/// Reference implementation: the embedded status server (feature `status-server`).
#[async_trait::async_trait]
pub trait AgentControlPlane: Send + Sync {
    /// Pause or resume an agent by name.
    async fn set_paused(&self, agent: &str, paused: bool) -> anyhow::Result<()>;

    /// Apply a partial configuration update to an agent.
    async fn update_config(&self, agent: &str, patch: ConfigPatch) -> anyhow::Result<()>;

    /// List buffered responses for an agent.
    async fn list_buffer(&self, agent: &str) -> anyhow::Result<Vec<BufferEntrySummary>>;

    /// Force-release a specific buffer entry (publish + ack).
    async fn release_buffer_entry(&self, agent: &str, entry_id: &str) -> anyhow::Result<()>;

    /// Reject (discard) a specific buffer entry (ack without publishing).
    async fn reject_buffer_entry(&self, agent: &str, entry_id: &str) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_patch_serde() {
        let patch = ConfigPatch {
            temperature: Some(0.9),
            frequency_penalty: Some(0.3),
            presence_penalty: Some(1.2),
            persona: Some("skeptical analyst".into()),
            textual_feedback: Some(false),
            max_react_iterations: Some(5),
            max_retries: Some(2),
        };
        let json = serde_json::to_string(&patch).unwrap();
        let roundtripped: ConfigPatch = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.temperature, Some(0.9));
        assert_eq!(roundtripped.persona, Some("skeptical analyst".into()));
        assert_eq!(roundtripped.max_retries, Some(2));
    }

    #[test]
    fn test_config_patch_all_none() {
        let patch = ConfigPatch::default();
        assert!(patch.temperature.is_none());
        assert!(patch.frequency_penalty.is_none());
        assert!(patch.presence_penalty.is_none());
        assert!(patch.persona.is_none());
        assert!(patch.textual_feedback.is_none());
        assert!(patch.max_react_iterations.is_none());
        assert!(patch.max_retries.is_none());

        // Empty patch serializes with all nulls
        let json = serde_json::to_string(&patch).unwrap();
        let roundtripped: ConfigPatch = serde_json::from_str(&json).unwrap();
        assert!(roundtripped.temperature.is_none());
    }

    #[test]
    fn test_config_patch_partial() {
        let json = r#"{"temperature": 0.5}"#;
        let patch: ConfigPatch = serde_json::from_str(json).unwrap();
        assert_eq!(patch.temperature, Some(0.5));
        assert!(patch.frequency_penalty.is_none());
        assert!(patch.persona.is_none());
    }

    #[test]
    fn test_config_patch_apply() {
        let mut config = crate::AgentConfig {
            name: "test".into(),
            provider_id: "p".into(),
            model_name: "m".into(),
            temperature: 0.7,
            frequency_penalty: Some(0.1),
            presence_penalty: Some(1.5),
            persona: Some("default persona".into()),
            textual_feedback: true,
            max_react_iterations: Some(10),
            max_retries: Some(3),
            ..Default::default()
        };

        let patch = ConfigPatch {
            temperature: Some(1.2),
            persona: Some("aggressive debater".into()),
            max_retries: Some(5),
            ..Default::default()
        };
        patch.apply(&mut config).expect("valid patch should apply");

        assert_eq!(config.temperature, 1.2);
        assert_eq!(config.persona, Some("aggressive debater".into()));
        assert_eq!(config.max_retries, Some(5));
        // Unpatched fields unchanged
        assert_eq!(config.frequency_penalty, Some(0.1));
        assert_eq!(config.presence_penalty, Some(1.5));
        assert!(config.textual_feedback);
        assert_eq!(config.max_react_iterations, Some(10));
    }

    // -------------------------------------------------------------------
    // ConfigPatch validation tests
    // -------------------------------------------------------------------

    #[test]
    fn test_validate_rejects_temperature_above_range() {
        let patch = ConfigPatch {
            temperature: Some(2.5),
            ..Default::default()
        };
        let err = patch.validate().unwrap_err();
        assert!(err.contains("temperature"), "err: {}", err);
    }

    #[test]
    fn test_validate_rejects_negative_temperature() {
        let patch = ConfigPatch {
            temperature: Some(-0.1),
            ..Default::default()
        };
        assert!(patch.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_nan_temperature() {
        let patch = ConfigPatch {
            temperature: Some(f32::NAN),
            ..Default::default()
        };
        assert!(patch.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_inf_temperature() {
        let patch = ConfigPatch {
            temperature: Some(f32::INFINITY),
            ..Default::default()
        };
        assert!(patch.validate().is_err());
    }

    #[test]
    fn test_validate_accepts_boundary_temperature() {
        let patch_zero = ConfigPatch {
            temperature: Some(0.0),
            ..Default::default()
        };
        assert!(patch_zero.validate().is_ok());
        let patch_two = ConfigPatch {
            temperature: Some(2.0),
            ..Default::default()
        };
        assert!(patch_two.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_frequency_penalty_out_of_range() {
        let patch = ConfigPatch {
            frequency_penalty: Some(3.0),
            ..Default::default()
        };
        let err = patch.validate().unwrap_err();
        assert!(err.contains("frequency_penalty"), "err: {}", err);
    }

    #[test]
    fn test_validate_rejects_presence_penalty_out_of_range() {
        let patch = ConfigPatch {
            presence_penalty: Some(-2.5),
            ..Default::default()
        };
        assert!(patch.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_max_react_iterations() {
        let patch = ConfigPatch {
            max_react_iterations: Some(0),
            ..Default::default()
        };
        let err = patch.validate().unwrap_err();
        assert!(err.contains("max_react_iterations"), "err: {}", err);
    }

    #[test]
    fn test_validate_rejects_negative_max_retries() {
        let patch = ConfigPatch {
            max_retries: Some(-1),
            ..Default::default()
        };
        let err = patch.validate().unwrap_err();
        assert!(err.contains("max_retries"), "err: {}", err);
    }

    #[test]
    fn test_validate_accepts_valid_patch() {
        let patch = ConfigPatch {
            temperature: Some(1.0),
            frequency_penalty: Some(-1.5),
            presence_penalty: Some(2.0),
            max_react_iterations: Some(5),
            max_retries: Some(0),
            ..Default::default()
        };
        assert!(patch.validate().is_ok());
    }

    #[test]
    fn test_validate_accepts_empty_patch() {
        let patch = ConfigPatch::default();
        assert!(patch.validate().is_ok());
    }

    #[test]
    fn test_apply_rejects_invalid_and_does_not_mutate() {
        let mut config = crate::AgentConfig {
            name: "test".into(),
            provider_id: "p".into(),
            model_name: "m".into(),
            temperature: 0.7,
            ..Default::default()
        };

        let patch = ConfigPatch {
            temperature: Some(5.0), // invalid
            persona: Some("should not be applied".into()),
            ..Default::default()
        };
        assert!(patch.apply(&mut config).is_err());
        // Config should be unchanged
        assert_eq!(config.temperature, 0.7);
        assert!(config.persona.is_none());
    }

    // -------------------------------------------------------------------
    // Boundary validation tests
    // -------------------------------------------------------------------

    /// Test that -2.0 and 2.0 are valid frequency_penalty values,
    /// but -2.01 and 2.01 are rejected.
    #[test]
    fn test_validate_frequency_penalty_exact_boundaries() {
        // -2.0 is valid (inclusive lower bound)
        let patch_lower = ConfigPatch {
            frequency_penalty: Some(-2.0),
            ..Default::default()
        };
        assert!(
            patch_lower.validate().is_ok(),
            "frequency_penalty -2.0 should be valid"
        );

        // 2.0 is valid (inclusive upper bound)
        let patch_upper = ConfigPatch {
            frequency_penalty: Some(2.0),
            ..Default::default()
        };
        assert!(
            patch_upper.validate().is_ok(),
            "frequency_penalty 2.0 should be valid"
        );

        // -2.01 is invalid (below lower bound)
        let patch_below = ConfigPatch {
            frequency_penalty: Some(-2.01),
            ..Default::default()
        };
        let err = patch_below.validate().unwrap_err();
        assert!(
            err.contains("frequency_penalty"),
            "error should mention frequency_penalty: {}",
            err
        );

        // 2.01 is invalid (above upper bound)
        let patch_above = ConfigPatch {
            frequency_penalty: Some(2.01),
            ..Default::default()
        };
        let err = patch_above.validate().unwrap_err();
        assert!(
            err.contains("frequency_penalty"),
            "error should mention frequency_penalty: {}",
            err
        );
    }

    /// Test that -2.0 and 2.0 are valid presence_penalty values,
    /// but -2.01 and 2.01 are rejected.
    #[test]
    fn test_validate_presence_penalty_exact_boundaries() {
        // -2.0 is valid (inclusive lower bound)
        let patch_lower = ConfigPatch {
            presence_penalty: Some(-2.0),
            ..Default::default()
        };
        assert!(
            patch_lower.validate().is_ok(),
            "presence_penalty -2.0 should be valid"
        );

        // 2.0 is valid (inclusive upper bound)
        let patch_upper = ConfigPatch {
            presence_penalty: Some(2.0),
            ..Default::default()
        };
        assert!(
            patch_upper.validate().is_ok(),
            "presence_penalty 2.0 should be valid"
        );

        // -2.01 is invalid (below lower bound)
        let patch_below = ConfigPatch {
            presence_penalty: Some(-2.01),
            ..Default::default()
        };
        let err = patch_below.validate().unwrap_err();
        assert!(
            err.contains("presence_penalty"),
            "error should mention presence_penalty: {}",
            err
        );

        // 2.01 is invalid (above upper bound)
        let patch_above = ConfigPatch {
            presence_penalty: Some(2.01),
            ..Default::default()
        };
        let err = patch_above.validate().unwrap_err();
        assert!(
            err.contains("presence_penalty"),
            "error should mention presence_penalty: {}",
            err
        );
    }

    /// Test that a very large value like i32::MAX is accepted for
    /// max_react_iterations (validation only requires >= 1).
    #[test]
    fn test_validate_max_react_iterations_large_value() {
        let patch = ConfigPatch {
            max_react_iterations: Some(i32::MAX),
            ..Default::default()
        };
        assert!(
            patch.validate().is_ok(),
            "max_react_iterations = i32::MAX ({}) should be valid",
            i32::MAX
        );
    }

    /// Test that persona = Some("") passes validation.
    ///
    /// The current validation does not reject empty strings for persona.
    /// This test documents this behavior: an empty persona is accepted
    /// at the ConfigPatch level; any semantic rejection (if desired)
    /// would happen at a higher layer.
    #[test]
    fn test_validate_persona_empty_string() {
        let patch = ConfigPatch {
            persona: Some("".into()),
            ..Default::default()
        };
        assert!(
            patch.validate().is_ok(),
            "empty persona string should pass validation"
        );

        // Also verify it applies correctly
        let mut config = crate::AgentConfig {
            name: "test".into(),
            provider_id: "p".into(),
            model_name: "m".into(),
            temperature: 0.7,
            persona: Some("original persona".into()),
            ..Default::default()
        };
        patch
            .apply(&mut config)
            .expect("empty persona patch should apply");
        assert_eq!(
            config.persona,
            Some("".into()),
            "persona should be set to empty string, not None"
        );
    }

    /// Exercises ALL ConfigPatch fields in apply(), ensuring every branch
    /// in the apply() method is covered — specifically frequency_penalty,
    /// presence_penalty, textual_feedback, and max_react_iterations.
    #[test]
    fn test_apply_all_config_patch_fields() {
        let mut config = crate::AgentConfig {
            name: "full-patch-test".into(),
            provider_id: "provider".into(),
            model_name: "model".into(),
            temperature: 0.5,
            frequency_penalty: None,
            presence_penalty: None,
            persona: None,
            textual_feedback: false,
            max_react_iterations: None,
            max_retries: None,
            ..Default::default()
        };

        // Patch ALL fields at once
        let patch = ConfigPatch {
            temperature: Some(1.5),
            frequency_penalty: Some(-0.5),
            presence_penalty: Some(0.8),
            persona: Some("devil's advocate".into()),
            textual_feedback: Some(true),
            max_react_iterations: Some(7),
            max_retries: Some(2),
        };
        patch.apply(&mut config).expect("full patch should apply");

        // Verify every field was applied
        assert_eq!(config.temperature, 1.5, "temperature should be updated");
        assert_eq!(
            config.frequency_penalty,
            Some(-0.5),
            "frequency_penalty should be set"
        );
        assert_eq!(
            config.presence_penalty,
            Some(0.8),
            "presence_penalty should be set"
        );
        assert_eq!(
            config.persona,
            Some("devil's advocate".into()),
            "persona should be set"
        );
        assert!(config.textual_feedback, "textual_feedback should be true");
        assert_eq!(
            config.max_react_iterations,
            Some(7),
            "max_react_iterations should be set"
        );
        assert_eq!(config.max_retries, Some(2), "max_retries should be set");

        // Now apply a second patch that only sets the previously-uncovered fields
        // to different values, verifying they overwrite correctly.
        let patch2 = ConfigPatch {
            frequency_penalty: Some(1.0),
            presence_penalty: Some(-1.0),
            textual_feedback: Some(false),
            max_react_iterations: Some(3),
            ..Default::default()
        };
        patch2
            .apply(&mut config)
            .expect("second patch should apply");

        assert_eq!(
            config.frequency_penalty,
            Some(1.0),
            "frequency_penalty should be overwritten"
        );
        assert_eq!(
            config.presence_penalty,
            Some(-1.0),
            "presence_penalty should be overwritten"
        );
        assert!(
            !config.textual_feedback,
            "textual_feedback should be toggled back to false"
        );
        assert_eq!(
            config.max_react_iterations,
            Some(3),
            "max_react_iterations should be overwritten"
        );
        // Fields not in patch2 should remain from patch1
        assert_eq!(
            config.temperature, 1.5,
            "temperature should be unchanged by patch2"
        );
        assert_eq!(
            config.persona,
            Some("devil's advocate".into()),
            "persona should be unchanged by patch2"
        );
    }
}
