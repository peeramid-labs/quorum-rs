use std::path::Path;

use crate::cli::tui::views::ConfigMutation;
use crate::cli::workspace::{RoomConfig, WorkspaceConfig};

/// Apply a config mutation atomically to the workspace config file.
///
/// Flow: clone → mutate clone → validate → write temp → atomic rename → swap.
/// In-memory config is only updated after successful disk write.
pub fn apply_mutation(
    config_path: &Path,
    config: &mut WorkspaceConfig,
    mutation: &ConfigMutation,
) -> Result<(), ConfigWriteError> {
    let mut new_cfg = config.clone();

    match mutation {
        ConfigMutation::AddRoom {
            name,
            policy,
            orchestrator,
        } => {
            if new_cfg.rooms.contains_key(name) {
                return Err(ConfigWriteError::RoomExists(name.clone()));
            }
            new_cfg.rooms.insert(
                name.clone(),
                RoomConfig {
                    policy: policy.clone(),
                    orchestrator: Some(orchestrator.clone()),
                },
            );
        }
        ConfigMutation::EditRoom {
            name,
            policy,
            orchestrator,
        } => {
            let room = new_cfg
                .rooms
                .get_mut(name)
                .ok_or_else(|| ConfigWriteError::RoomNotFound(name.clone()))?;
            if let Some(p) = policy {
                room.policy = p.clone();
            }
            if let Some(o) = orchestrator {
                room.orchestrator = Some(o.clone());
            }
        }
        ConfigMutation::SetDefaultRoom(name) => {
            if !new_cfg.rooms.contains_key(name) {
                return Err(ConfigWriteError::RoomNotFound(name.clone()));
            }
            new_cfg.default_room = Some(name.clone());
        }
    }

    // Validate before writing
    new_cfg
        .validate()
        .map_err(|e| ConfigWriteError::Validation(e.to_string()))?;

    // Write to temp file then atomically rename
    let yaml =
        serde_yaml::to_string(&new_cfg).map_err(|e| ConfigWriteError::Serialize(e.to_string()))?;

    let tmp_path = config_path.with_extension("yaml.tmp");
    std::fs::write(&tmp_path, &yaml).map_err(|e| ConfigWriteError::Io(e.to_string()))?;
    std::fs::rename(&tmp_path, config_path).map_err(|e| ConfigWriteError::Io(e.to_string()))?;

    // Only update in-memory config after successful disk write.
    *config = new_cfg;

    Ok(())
}

/// Errors from config write operations.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConfigWriteError {
    #[error("Room '{0}' already exists")]
    RoomExists(String),
    #[error("Room '{0}' not found")]
    RoomNotFound(String),
    #[error("Config validation failed: {0}")]
    Validation(String),
    #[error("Serialization error: {0}")]
    Serialize(String),
    #[error("IO error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn minimal_config() -> WorkspaceConfig {
        WorkspaceConfig {
            policies: {
                let mut m = HashMap::new();
                m.insert(
                    "review".into(),
                    crate::cli::workspace::PolicyConfig {
                        deliberation_type: None,
                        agents: Some(vec!["Agent1".into(), "Agent2".into()]),
                        roles: None,
                        max_rounds: 2,
                        effort: 0.85,
                        sla: None,
                        capabilities: None,
                        tags: None,
                        mode: Default::default(),
                    },
                );
                m
            },
            orchestrators: {
                let mut m = HashMap::new();
                m.insert(
                    "local".into(),
                    crate::cli::workspace::OrchestratorConfig {
                        mode: None,
                        address: None,
                        token: None,
                        nats_url: None,
                        config_file: Some("config.yml".into()),
                    },
                );
                m
            },
            rooms: HashMap::new(),
            shared: None,
            default_room: None,
            agents: None,
        }
    }

    #[test]
    fn add_room_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nsed.yaml");
        let mut config = minimal_config();

        let mutation = ConfigMutation::AddRoom {
            name: "test-room".into(),
            policy: "review".into(),
            orchestrator: "local".into(),
        };

        // Write initial config so file exists
        let yaml = serde_yaml::to_string(&config).unwrap();
        std::fs::write(&path, &yaml).unwrap();

        apply_mutation(&path, &mut config, &mutation).unwrap();

        assert!(config.rooms.contains_key("test-room"));
        assert_eq!(config.rooms["test-room"].policy, "review");

        // Verify file was written
        let reloaded: WorkspaceConfig =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(reloaded.rooms.contains_key("test-room"));
    }

    #[test]
    fn add_room_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nsed.yaml");
        let mut config = minimal_config();
        config.rooms.insert(
            "existing".into(),
            RoomConfig {
                policy: "review".into(),
                orchestrator: Some("local".into()),
            },
        );

        let mutation = ConfigMutation::AddRoom {
            name: "existing".into(),
            policy: "review".into(),
            orchestrator: "local".into(),
        };

        let result = apply_mutation(&path, &mut config, &mutation);
        assert!(matches!(result, Err(ConfigWriteError::RoomExists(_))));
    }

    #[test]
    fn edit_room_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nsed.yaml");
        let mut config = minimal_config();
        config.rooms.insert(
            "my-room".into(),
            RoomConfig {
                policy: "review".into(),
                orchestrator: Some("local".into()),
            },
        );

        let yaml = serde_yaml::to_string(&config).unwrap();
        std::fs::write(&path, &yaml).unwrap();

        let mutation = ConfigMutation::EditRoom {
            name: "my-room".into(),
            policy: Some("review".into()),
            orchestrator: None,
        };

        apply_mutation(&path, &mut config, &mutation).unwrap();
        assert_eq!(config.rooms["my-room"].policy, "review");
    }

    #[test]
    fn edit_room_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nsed.yaml");
        let mut config = minimal_config();

        let mutation = ConfigMutation::EditRoom {
            name: "nonexistent".into(),
            policy: Some("review".into()),
            orchestrator: None,
        };

        let result = apply_mutation(&path, &mut config, &mutation);
        assert!(matches!(result, Err(ConfigWriteError::RoomNotFound(_))));
    }

    #[test]
    fn set_default_room_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nsed.yaml");
        let mut config = minimal_config();
        config.rooms.insert(
            "my-room".into(),
            RoomConfig {
                policy: "review".into(),
                orchestrator: Some("local".into()),
            },
        );

        let yaml = serde_yaml::to_string(&config).unwrap();
        std::fs::write(&path, &yaml).unwrap();

        let mutation = ConfigMutation::SetDefaultRoom("my-room".into());
        apply_mutation(&path, &mut config, &mutation).unwrap();
        assert_eq!(config.default_room.as_deref(), Some("my-room"));
    }

    #[test]
    fn set_default_room_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nsed.yaml");
        let mut config = minimal_config();

        let mutation = ConfigMutation::SetDefaultRoom("nonexistent".into());
        let result = apply_mutation(&path, &mut config, &mutation);
        assert!(matches!(result, Err(ConfigWriteError::RoomNotFound(_))));
    }

    #[test]
    fn write_failure_does_not_mutate_in_memory_config() {
        // Use a path that doesn't exist to trigger write failure
        let path = Path::new("/nonexistent/dir/nsed.yaml");
        let mut config = minimal_config();
        let original_rooms_count = config.rooms.len();

        let mutation = ConfigMutation::AddRoom {
            name: "new-room".into(),
            policy: "review".into(),
            orchestrator: "local".into(),
        };

        let result = apply_mutation(path, &mut config, &mutation);
        assert!(result.is_err());
        // In-memory config should NOT have been mutated
        assert_eq!(config.rooms.len(), original_rooms_count);
        assert!(!config.rooms.contains_key("new-room"));
    }
}
