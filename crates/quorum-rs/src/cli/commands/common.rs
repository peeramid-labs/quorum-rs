use crate::cli::remote::RemoteOrchestrator;
use crate::cli::workspace::{OrchestratorConfig, OrchestratorMode, WorkspaceConfig};

/// Resolve which orchestrators to target.
///
/// - If `name` is given, return that single orchestrator (error if missing).
/// - Otherwise return all remote orchestrators in the config.
pub fn resolve_orchestrators<'a>(
    config: &'a WorkspaceConfig,
    name: Option<&'a str>,
) -> Result<Vec<(&'a str, &'a OrchestratorConfig)>, String> {
    if let Some(name) = name {
        let orch = config
            .orchestrators
            .get(name)
            .ok_or_else(|| format!("unknown orchestrator '{name}'"))?;
        return Ok(vec![(name, orch)]);
    }

    let remotes: Vec<_> = config
        .orchestrators
        .iter()
        .filter(|(_, o)| o.mode.as_ref() == Some(&OrchestratorMode::Remote))
        .map(|(n, o)| (n.as_str(), o))
        .collect();

    if remotes.is_empty() {
        return Err("no remote orchestrators configured".to_string());
    }

    Ok(remotes)
}

/// Resolve a single orchestrator for commands that need exactly one (e.g. trace).
///
/// Priority: `--orchestrator` flag → default_room's orchestrator → error.
pub fn resolve_single_orchestrator<'a>(
    config: &'a WorkspaceConfig,
    name: Option<&'a str>,
) -> Result<(&'a str, &'a OrchestratorConfig), String> {
    if let Some(name) = name {
        let orch = config
            .orchestrators
            .get(name)
            .ok_or_else(|| format!("unknown orchestrator '{name}'"))?;
        return Ok((name, orch));
    }

    // Fall back to default_room's orchestrator
    let (_, room) = config
        .resolve_room(None)
        .map_err(|e| format!("cannot determine orchestrator: {e}"))?;

    let orch_name = match &room.orchestrator {
        Some(name) => name.as_str(),
        None => {
            if config.orchestrators.len() == 1 {
                config.orchestrators.keys().next().unwrap().as_str()
            } else {
                return Err(
                    "room does not specify an orchestrator and multiple are configured; \
                     use --orchestrator to select one"
                        .to_string(),
                );
            }
        }
    };

    let orch = config
        .orchestrators
        .get(orch_name)
        .ok_or_else(|| format!("unknown orchestrator '{orch_name}'"))?;

    Ok((orch_name, orch))
}

/// Build a `RemoteOrchestrator` client from config, resolving env tokens.
pub fn build_remote(name: &str, orch: &OrchestratorConfig) -> Result<RemoteOrchestrator, String> {
    RemoteOrchestrator::from_config(name, orch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::workspace::{OrchestratorConfig, OrchestratorMode, PolicyConfig, RoomConfig};
    use std::collections::HashMap;

    fn make_config(orchestrators: Vec<(&str, OrchestratorConfig)>) -> WorkspaceConfig {
        let mut orch_map = HashMap::new();
        for (name, orch) in orchestrators {
            orch_map.insert(name.to_string(), orch);
        }
        WorkspaceConfig {
            policies: HashMap::from([(
                "p".into(),
                PolicyConfig {
                    deliberation_type: None,
                    agents: Some(vec!["a".into(), "b".into()]),
                    roles: None,
                    max_rounds: 2,
                    effort: 0.85,
                    sla: None,
                    capabilities: None,
                    tags: None,
                    mode: Default::default(),
                },
            )]),
            orchestrators: orch_map,
            rooms: HashMap::from([(
                "default".into(),
                RoomConfig {
                    policy: "p".into(),
                    orchestrator: Some("prod".into()),
                },
            )]),
            default_room: Some("default".into()),
            shared: None,
            agents: None,
        }
    }

    fn remote_orch() -> OrchestratorConfig {
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Remote),
            address: Some("http://example.com".into()),
            token: Some("tok".into()),
            nats_url: None,
            config_file: None,
        }
    }

    fn embedded_orch() -> OrchestratorConfig {
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Embedded),
            address: None,
            token: None,
            nats_url: None,
            config_file: None,
        }
    }

    #[test]
    fn resolve_single_by_name() {
        let config = make_config(vec![("prod", remote_orch()), ("local", embedded_orch())]);
        let (name, _) = resolve_orchestrators(&config, Some("prod")).unwrap()[0];
        assert_eq!(name, "prod");
    }

    #[test]
    fn resolve_unknown_name_errors() {
        let config = make_config(vec![("prod", remote_orch())]);
        let err = resolve_orchestrators(&config, Some("nope")).unwrap_err();
        assert!(err.contains("unknown orchestrator 'nope'"), "got: {err}");
    }

    #[test]
    fn resolve_all_filters_remote_only() {
        let config = make_config(vec![("prod", remote_orch()), ("local", embedded_orch())]);
        let result = resolve_orchestrators(&config, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "prod");
    }

    #[test]
    fn resolve_no_remotes_errors() {
        let config = make_config(vec![("local", embedded_orch())]);
        let err = resolve_orchestrators(&config, None).unwrap_err();
        assert!(err.contains("no remote"), "got: {err}");
    }

    #[test]
    fn resolve_single_orchestrator_by_flag() {
        let config = make_config(vec![("prod", remote_orch())]);
        let (name, _) = resolve_single_orchestrator(&config, Some("prod")).unwrap();
        assert_eq!(name, "prod");
    }

    #[test]
    fn resolve_single_orchestrator_unknown_errors() {
        let config = make_config(vec![("prod", remote_orch())]);
        let err = resolve_single_orchestrator(&config, Some("nope")).unwrap_err();
        assert!(err.contains("unknown orchestrator"), "got: {err}");
    }

    #[test]
    fn resolve_single_orchestrator_from_default_room() {
        let config = make_config(vec![("prod", remote_orch())]);
        let (name, _) = resolve_single_orchestrator(&config, None).unwrap();
        assert_eq!(name, "prod");
    }

    #[test]
    fn build_remote_success() {
        let orch = remote_orch();
        assert!(build_remote("prod", &orch).is_ok());
    }

    #[test]
    fn build_remote_missing_address() {
        let mut orch = remote_orch();
        orch.address = None;
        match build_remote("prod", &orch) {
            Err(e) => assert!(e.contains("missing address"), "got: {e}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_remote_missing_token() {
        let mut orch = remote_orch();
        orch.token = None;
        match build_remote("prod", &orch) {
            Err(e) => assert!(e.contains("missing token"), "got: {e}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_remote_empty_address_after_expansion() {
        let mut orch = remote_orch();
        orch.address = Some("${NONEXISTENT_ADDR_VAR_12345}".into());
        match build_remote("prod", &orch) {
            Err(e) => assert!(e.contains("empty address"), "got: {e}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_remote_rejects_embedded_orchestrator() {
        let orch = embedded_orch();
        match build_remote("local", &orch) {
            Err(e) => assert!(e.contains("not a remote orchestrator"), "got: {e}"),
            Ok(_) => panic!("expected error for embedded orchestrator"),
        }
    }
}
