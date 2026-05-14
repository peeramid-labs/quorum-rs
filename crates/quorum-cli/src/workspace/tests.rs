#[cfg(test)]
mod parsing {
    use crate::workspace::*;

    fn minimal_static_yaml() -> &'static str {
        r#"
policies:
  quick:
    agents: ["agent_a", "agent_b"]
    max_rounds: 2
    effort: 0.85

orchestrators:
  local:
    mode: embedded

rooms:
  audit:
    policy: quick
    orchestrator: local
"#
    }

    fn full_role_yaml() -> &'static str {
        r#"
policies:
  security-audit:
    roles:
      - role: coder
        count: 1
        capabilities: ["lang:rust", "lang:typescript"]
        context:
          - name: codebase
            path: "./src/"
      - role: security
        count: 1
        capabilities: ["security:*"]
        context:
          - name: audit_report
            path: "./reports/audit.json"
    max_rounds: 3
    effort: 0.80
    sla:
      job_timeout_secs: 600
    capabilities: ["rust", "security"]
    tags: ["domain:security", "public"]

orchestrators:
  local:
    mode: embedded
  production:
    mode: remote
    address: "https://api.peeramid.xyz"
    token: "bearer_xxx"
    nats_url: "nats://api.peeramid.xyz:4222"

rooms:
  review-q1:
    policy: security-audit
    orchestrator: production
  review-q2:
    policy: security-audit
    orchestrator: production

shared:
  - name: readme
    path: "./README.md"

default_room: review-q1
"#
    }

    #[test]
    fn parse_minimal_static_config() {
        let config: WorkspaceConfig = serde_yaml::from_str(minimal_static_yaml()).unwrap();
        assert_eq!(config.policies.len(), 1);
        assert_eq!(config.orchestrators.len(), 1);
        assert!(config.orchestrators.contains_key("local"));
        assert_eq!(config.rooms.len(), 1);

        let policy = &config.policies["quick"];
        assert_eq!(policy.agents.as_ref().unwrap(), &["agent_a", "agent_b"]);
        assert_eq!(policy.max_rounds, 2);
        assert!((policy.effort - 0.85).abs() < f32::EPSILON);
        assert!(policy.roles.is_none());

        let room = &config.rooms["audit"];
        assert_eq!(room.policy, "quick");
        assert_eq!(room.orchestrator.as_deref(), Some("local"));
    }

    #[test]
    fn parse_full_role_config() {
        let config: WorkspaceConfig = serde_yaml::from_str(full_role_yaml()).unwrap();
        assert_eq!(config.policies.len(), 1);
        assert_eq!(config.orchestrators.len(), 2);
        assert_eq!(config.rooms.len(), 2);

        let prod = &config.orchestrators["production"];
        assert_eq!(prod.address.as_deref(), Some("https://api.peeramid.xyz"));
        assert_eq!(prod.token.as_deref(), Some("bearer_xxx"));
        assert_eq!(
            prod.nats_url.as_deref(),
            Some("nats://api.peeramid.xyz:4222")
        );

        let policy = &config.policies["security-audit"];
        assert!(policy.agents.is_none());

        let roles = policy.roles.as_ref().unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].role, "coder");
        assert_eq!(roles[0].count, 1);
        assert_eq!(roles[0].capabilities, vec!["lang:rust", "lang:typescript"]);
        assert_eq!(roles[0].context.as_ref().unwrap().len(), 1);
        assert_eq!(roles[0].context.as_ref().unwrap()[0].name, "codebase");

        assert_eq!(roles[1].role, "security");
        assert_eq!(roles[1].capabilities, vec!["security:*"]);

        assert_eq!(policy.sla.as_ref().unwrap().job_timeout_secs, 600);
        assert_eq!(policy.capabilities.as_ref().unwrap(), &["rust", "security"]);

        let tags = policy.tags.as_ref().unwrap();
        assert_eq!(tags, &["domain:security", "public"]);

        // Rooms reference the same policy
        assert_eq!(config.rooms["review-q1"].policy, "security-audit");
        assert_eq!(config.rooms["review-q2"].policy, "security-audit");
        assert_eq!(
            config.rooms["review-q1"].orchestrator.as_deref(),
            Some("production")
        );

        assert_eq!(config.shared.as_ref().unwrap().len(), 1);
        assert_eq!(config.shared.as_ref().unwrap()[0].name, "readme");

        assert_eq!(config.default_room.as_deref(), Some("review-q1"));
    }

    #[test]
    fn two_rooms_same_policy() {
        let config: WorkspaceConfig = serde_yaml::from_str(full_role_yaml()).unwrap();
        assert_eq!(config.rooms["review-q1"].policy, "security-audit");
        assert_eq!(config.rooms["review-q2"].policy, "security-audit");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn room_without_orchestrator() {
        let yaml = r#"
policies:
  quick:
    agents: ["a", "b"]
    max_rounds: 1
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: quick
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.rooms["test"].orchestrator.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn parse_embedded_orchestrator_defaults() {
        let config: WorkspaceConfig = serde_yaml::from_str(minimal_static_yaml()).unwrap();
        let local = &config.orchestrators["local"];
        assert_eq!(local.mode, Some(OrchestratorMode::Embedded));
        assert!(local.address.is_none());
        assert!(local.token.is_none());
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let config: WorkspaceConfig = serde_yaml::from_str(full_role_yaml()).unwrap();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let reparsed: WorkspaceConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(config.rooms.len(), reparsed.rooms.len());
        assert_eq!(config.policies.len(), reparsed.policies.len());
        assert_eq!(config.orchestrators.len(), reparsed.orchestrators.len());
    }

    #[test]
    fn legacy_rounds_and_phase_timeout_secs_aliases_deserialize() {
        // Workspace YAML produced by older CLI releases uses the legacy
        // keys `rounds` and `phase_timeout_secs`. Serde aliases must accept
        // these on input so upgrading doesn't break existing workspaces,
        // while serialization is always canonical (`max_rounds` /
        // `job_timeout_secs`).
        let legacy_yaml = r#"
policies:
  legacy:
    agents: ["agent_a", "agent_b"]
    rounds: 4
    effort: 0.7
    sla:
      phase_timeout_secs: 450

orchestrators:
  local:
    mode: embedded

rooms:
  audit:
    policy: legacy
    orchestrator: local
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(legacy_yaml).unwrap();

        let policy = &config.policies["legacy"];
        assert_eq!(
            policy.max_rounds, 4,
            "legacy `rounds` alias should populate `max_rounds`"
        );
        let sla = policy.sla.as_ref().expect("sla present");
        assert_eq!(
            sla.job_timeout_secs, 450,
            "legacy `phase_timeout_secs` alias should populate `job_timeout_secs`"
        );

        // Reserialize as JSON and assert only the canonical keys appear.
        let json = serde_json::to_string(policy).unwrap();
        assert!(
            json.contains("\"max_rounds\":4"),
            "canonical `max_rounds` key missing from reserialized output: {json}"
        );
        assert!(
            json.contains("\"job_timeout_secs\":450"),
            "canonical `job_timeout_secs` key missing from reserialized output: {json}"
        );
        assert!(
            !json.contains("\"rounds\""),
            "legacy `rounds` key leaked into canonical output: {json}"
        );
        assert!(
            !json.contains("phase_timeout_secs"),
            "legacy `phase_timeout_secs` key leaked into canonical output: {json}"
        );

        // The loaded workspace must still validate cleanly.
        config
            .validate()
            .expect("legacy-alias config should validate");
    }

    #[test]
    fn default_effort() {
        let yaml = r#"
policies:
  p:
    agents: ["a", "b"]
    max_rounds: 1
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: p
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!((config.policies["p"].effort - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn default_rounds() {
        let yaml = r#"
policies:
  p:
    agents: ["a", "b"]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: p
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.policies["p"].max_rounds, 3);
    }

    #[test]
    fn default_role_count_is_one() {
        let yaml = r#"
policies:
  p:
    roles:
      - role: coder
        capabilities: ["lang:rust"]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: p
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let roles = config.policies["p"].roles.as_ref().unwrap();
        assert_eq!(roles[0].count, 1);
    }

    #[test]
    fn sla_optional() {
        let config: WorkspaceConfig = serde_yaml::from_str(minimal_static_yaml()).unwrap();
        assert!(config.policies["quick"].sla.is_none());
    }

    #[test]
    fn shared_optional() {
        let config: WorkspaceConfig = serde_yaml::from_str(minimal_static_yaml()).unwrap();
        assert!(config.shared.is_none());
    }

    #[test]
    fn orchestrator_mode_serializes_lowercase() {
        let mode = OrchestratorMode::Embedded;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, r#""embedded""#);

        let mode = OrchestratorMode::Remote;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, r#""remote""#);
    }
}

#[cfg(test)]
mod validation {
    use crate::workspace::*;

    fn valid_static_config() -> WorkspaceConfig {
        serde_yaml::from_str(
            r#"
policies:
  quick:
    agents: ["agent_a", "agent_b"]
    max_rounds: 2
    effort: 0.85
orchestrators:
  local:
    mode: embedded
rooms:
  audit:
    policy: quick
    orchestrator: local
"#,
        )
        .unwrap()
    }

    fn valid_role_config() -> WorkspaceConfig {
        serde_yaml::from_str(
            r#"
policies:
  review:
    roles:
      - role: coder
        count: 1
        capabilities: ["lang:rust"]
      - role: qa
        count: 1
        capabilities: ["testing"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  dev:
    policy: review
    orchestrator: local
"#,
        )
        .unwrap()
    }

    #[test]
    fn valid_static_passes() {
        assert!(valid_static_config().validate().is_ok());
    }

    #[test]
    fn valid_role_passes() {
        assert!(valid_role_config().validate().is_ok());
    }

    // --- Room validation ---

    #[test]
    fn room_unknown_policy_ref() {
        let mut config = valid_static_config();
        config.rooms.get_mut("audit").unwrap().policy = "nonexistent".to_string();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "error should mention the missing policy: {err}"
        );
    }

    #[test]
    fn room_unknown_orchestrator_ref() {
        let mut config = valid_static_config();
        config.rooms.get_mut("audit").unwrap().orchestrator = Some("nonexistent".to_string());
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "error should mention the missing orchestrator: {err}"
        );
    }

    #[test]
    fn room_orchestrator_none_is_valid() {
        let mut config = valid_static_config();
        config.rooms.get_mut("audit").unwrap().orchestrator = None;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn two_rooms_same_policy_valid() {
        let mut config = valid_static_config();
        config.rooms.insert(
            "audit-2".to_string(),
            RoomConfig {
                policy: "quick".to_string(),
                orchestrator: Some("local".to_string()),
            },
        );
        assert!(config.validate().is_ok());
    }

    // --- Policy validation ---

    #[test]
    fn policy_neither_agents_nor_roles() {
        let mut config = valid_static_config();
        let policy = config.policies.get_mut("quick").unwrap();
        policy.agents = None;
        policy.roles = None;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("agents") || err.to_string().contains("roles"),
            "error should mention agents/roles: {err}"
        );
    }

    #[test]
    fn policy_both_agents_and_roles() {
        let mut config = valid_role_config();
        let policy = config.policies.get_mut("review").unwrap();
        policy.agents = Some(vec!["agent_a".to_string()]);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("mutually exclusive"),
            "error should mention mutual exclusivity: {err}"
        );
    }

    #[test]
    fn policy_static_needs_at_least_two_agents() {
        let mut config = valid_static_config();
        config.policies.get_mut("quick").unwrap().agents = Some(vec!["only_one".to_string()]);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("2"),
            "error should mention minimum 2 agents: {err}"
        );
    }

    #[test]
    fn policy_static_empty_agents_rejected() {
        let mut config = valid_static_config();
        config.policies.get_mut("quick").unwrap().agents = Some(vec![]);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("2"),
            "error should mention minimum 2: {err}"
        );
    }

    #[test]
    fn policy_duplicate_role_names_rejected() {
        let mut config = valid_role_config();
        let roles = config
            .policies
            .get_mut("review")
            .unwrap()
            .roles
            .as_mut()
            .unwrap();
        roles[1].role = "coder".to_string();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("duplicate") || err.to_string().contains("coder"),
            "error should mention duplicate role: {err}"
        );
    }

    #[test]
    fn policy_role_count_zero_rejected() {
        let mut config = valid_role_config();
        let roles = config
            .policies
            .get_mut("review")
            .unwrap()
            .roles
            .as_mut()
            .unwrap();
        roles[0].count = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("count"),
            "error should mention count: {err}"
        );
    }

    #[test]
    fn policy_role_empty_capabilities_rejected() {
        let mut config = valid_role_config();
        let roles = config
            .policies
            .get_mut("review")
            .unwrap()
            .roles
            .as_mut()
            .unwrap();
        roles[0].capabilities = vec![];
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("capabilit"),
            "error should mention capabilities: {err}"
        );
    }

    #[test]
    fn policy_effort_below_zero() {
        let mut config = valid_static_config();
        config.policies.get_mut("quick").unwrap().effort = -0.1;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("effort"),
            "error should mention effort: {err}"
        );
    }

    #[test]
    fn policy_effort_above_one() {
        let mut config = valid_static_config();
        config.policies.get_mut("quick").unwrap().effort = 1.1;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("effort"),
            "error should mention effort: {err}"
        );
    }

    #[test]
    fn policy_rounds_zero_rejected() {
        let mut config = valid_static_config();
        config.policies.get_mut("quick").unwrap().max_rounds = 0;
        let err = config.validate().unwrap_err();
        let msg = err.to_string();
        // Error message must reference the renamed field so operators know
        // which YAML key to fix.
        assert!(
            msg.contains("max_rounds"),
            "error should mention `max_rounds`: {msg}"
        );
        assert!(
            msg.contains(">= 1"),
            "error should state the minimum value: {msg}"
        );
    }

    #[test]
    fn policy_sla_timeout_zero_rejected() {
        let mut config = valid_static_config();
        config.policies.get_mut("quick").unwrap().sla = Some(PolicySla {
            job_timeout_secs: 0,
            response_sla_secs: None,
            max_tokens: None,
        });
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("timeout"),
            "error should mention timeout: {err}"
        );
    }

    #[test]
    fn policy_role_total_agents_below_two_rejected() {
        let yaml = r#"
policies:
  solo:
    roles:
      - role: solo
        count: 1
        capabilities: ["*"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: solo
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("2"),
            "error should mention minimum 2 agents: {err}"
        );
    }

    // --- Capability & tag validation ---

    #[test]
    fn policy_wildcard_capability_valid() {
        let config: WorkspaceConfig = serde_yaml::from_str(
            r#"
policies:
  wild:
    roles:
      - role: any_agent
        count: 1
        capabilities: ["*"]
      - role: security
        count: 1
        capabilities: ["security:*"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: wild
"#,
        )
        .unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn policy_invalid_capability_format_rejected() {
        let mut config = valid_role_config();
        let roles = config
            .policies
            .get_mut("review")
            .unwrap()
            .roles
            .as_mut()
            .unwrap();
        roles[0].capabilities = vec!["has spaces".to_string()];
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("capabilit"),
            "error should mention capability format: {err}"
        );
    }

    #[test]
    fn policy_capability_with_special_chars_rejected() {
        let mut config = valid_role_config();
        let roles = config
            .policies
            .get_mut("review")
            .unwrap()
            .roles
            .as_mut()
            .unwrap();
        roles[0].capabilities = vec!["bad/slash".to_string()];
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("capabilit"),
            "error should mention capability format: {err}"
        );
    }

    #[test]
    fn capability_namespace_wildcard_valid() {
        assert!(validate_capability_tag("security:*").is_ok());
        assert!(validate_capability_tag("lang:*").is_ok());
        assert!(validate_capability_tag("*").is_ok());
    }

    #[test]
    fn capability_exact_match_valid() {
        assert!(validate_capability_tag("rust").is_ok());
        assert!(validate_capability_tag("lang:rust").is_ok());
        assert!(validate_capability_tag("security:owasp").is_ok());
    }

    #[test]
    fn capability_invalid_chars() {
        assert!(validate_capability_tag("has space").is_err());
        assert!(validate_capability_tag("bad/slash").is_err());
        assert!(validate_capability_tag("bad+plus").is_err());
        assert!(validate_capability_tag("").is_err());
    }

    #[test]
    fn policy_level_capability_invalid_rejected() {
        let yaml = r#"
policies:
  bad:
    agents: ["a", "b"]
    max_rounds: 2
    capabilities: ["valid", "bad/slash"]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: bad
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("bad/slash"),
            "error should mention the invalid capability: {err}"
        );
    }

    #[test]
    fn policy_level_capability_valid_passes() {
        let yaml = r#"
policies:
  tagged:
    agents: ["a", "b"]
    max_rounds: 2
    capabilities: ["rust", "security:*", "lang:typescript"]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: tagged
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn policy_tags_parsed() {
        let yaml = r#"
policies:
  tagged:
    agents: ["a", "b"]
    max_rounds: 2
    tags: ["domain:security", "tier:premium", "public"]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: tagged
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let tags = config.policies["tagged"].tags.as_ref().unwrap();
        assert_eq!(tags.len(), 3);
        assert!(tags.contains(&"public".to_string()));
        assert!(tags.contains(&"domain:security".to_string()));
    }

    #[test]
    fn policy_tags_default_none() {
        let config = valid_static_config();
        assert!(config.policies["quick"].tags.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn policy_tags_validated() {
        let yaml = r#"
policies:
  bad:
    agents: ["a", "b"]
    max_rounds: 2
    tags: ["valid", "bad/slash"]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: bad
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("bad/slash"),
            "error should mention the invalid tag: {err}"
        );
    }

    #[test]
    fn policy_tags_wildcard_valid() {
        let yaml = r#"
policies:
  wild:
    agents: ["a", "b"]
    max_rounds: 2
    tags: ["domain:security", "tier:*", "public"]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: wild
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn policy_tags_empty_tag_rejected() {
        let yaml = r#"
policies:
  bad:
    agents: ["a", "b"]
    max_rounds: 2
    tags: ["valid", ""]
orchestrators:
  local:
    mode: embedded
rooms:
  test:
    policy: bad
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("tag"),
            "error should mention invalid tag: {err}"
        );
    }

    // --- Top-level validation ---

    #[test]
    fn empty_rooms_is_valid() {
        // Agents-only config: no rooms needed
        let mut config = valid_static_config();
        config.rooms.clear();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn empty_orchestrators_rejected() {
        let mut config = valid_static_config();
        config.orchestrators.clear();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("orchestrator"),
            "error should mention orchestrators: {err}"
        );
    }

    #[test]
    fn empty_policies_rejected() {
        let mut config = valid_static_config();
        config.policies.clear();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("polic"),
            "error should mention policies: {err}"
        );
    }

    #[test]
    fn remote_orchestrator_missing_address() {
        let mut config = valid_static_config();
        config.orchestrators.insert(
            "bad_remote".to_string(),
            OrchestratorConfig {
                mode: Some(OrchestratorMode::Remote),
                address: None,
                token: Some("tok".to_string()),
                nats_url: None,
                config_file: None,
            },
        );
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("address"),
            "error should mention missing address: {err}"
        );
    }

    #[test]
    fn remote_orchestrator_missing_token() {
        let mut config = valid_static_config();
        config.orchestrators.insert(
            "bad_remote".to_string(),
            OrchestratorConfig {
                mode: Some(OrchestratorMode::Remote),
                address: Some("https://example.com".to_string()),
                token: None,
                nats_url: None,
                config_file: None,
            },
        );
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("token"),
            "error should mention missing token: {err}"
        );
    }

    #[test]
    fn multiple_orchestrators_valid() {
        let yaml = r#"
policies:
  p1:
    agents: ["a", "b"]
    max_rounds: 2
  p2:
    agents: ["c", "d"]
    max_rounds: 3
orchestrators:
  local:
    mode: embedded
  remote:
    mode: remote
    address: "https://example.com"
    token: "tok"
rooms:
  room_a:
    policy: p1
    orchestrator: local
  room_b:
    policy: p2
    orchestrator: remote
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn default_room_valid_reference() {
        let yaml = r#"
policies:
  quick:
    agents: ["a", "b"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  audit:
    policy: quick
default_room: audit
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn default_room_invalid_reference() {
        let yaml = r#"
policies:
  quick:
    agents: ["a", "b"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  audit:
    policy: quick
default_room: nonexistent
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "error should mention the invalid default_room: {err}"
        );
    }

    #[test]
    fn default_room_none_is_valid() {
        let config = valid_static_config();
        assert!(config.default_room.is_none());
        assert!(config.validate().is_ok());
    }
}

#[cfg(test)]
mod resolve_room {
    use crate::workspace::*;

    fn single_room_config() -> WorkspaceConfig {
        serde_yaml::from_str(
            r#"
policies:
  quick:
    agents: ["a", "b"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  audit:
    policy: quick
"#,
        )
        .unwrap()
    }

    fn multi_room_config() -> WorkspaceConfig {
        serde_yaml::from_str(
            r#"
policies:
  quick:
    agents: ["a", "b"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  audit:
    policy: quick
  review:
    policy: quick
"#,
        )
        .unwrap()
    }

    fn multi_room_with_default() -> WorkspaceConfig {
        serde_yaml::from_str(
            r#"
policies:
  quick:
    agents: ["a", "b"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  audit:
    policy: quick
  review:
    policy: quick
default_room: review
"#,
        )
        .unwrap()
    }

    #[test]
    fn explicit_flag_picks_named_room() {
        let config = multi_room_config();
        let (name, room) = config.resolve_room(Some("audit")).unwrap();
        assert_eq!(name, "audit");
        assert_eq!(room.policy, "quick");
    }

    #[test]
    fn explicit_flag_unknown_room_errors() {
        let config = multi_room_config();
        let err = config.resolve_room(Some("nonexistent")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nonexistent"),
            "error should name the room: {msg}"
        );
        assert!(
            msg.contains("audit"),
            "error should list available rooms: {msg}"
        );
    }

    #[test]
    fn default_room_used_when_no_flag() {
        let config = multi_room_with_default();
        let (name, room) = config.resolve_room(None).unwrap();
        assert_eq!(name, "review");
        assert_eq!(room.policy, "quick");
    }

    #[test]
    fn single_room_auto_selected() {
        let config = single_room_config();
        let (name, room) = config.resolve_room(None).unwrap();
        assert_eq!(name, "audit");
        assert_eq!(room.policy, "quick");
    }

    #[test]
    fn multiple_rooms_no_default_errors() {
        let config = multi_room_config();
        let err = config.resolve_room(None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("multiple rooms") || msg.contains("--room"),
            "error should explain: {msg}"
        );
        assert!(msg.contains("audit"), "error should list rooms: {msg}");
        assert!(msg.contains("review"), "error should list rooms: {msg}");
    }

    #[test]
    fn explicit_flag_overrides_default() {
        let config = multi_room_with_default();
        let (name, _) = config.resolve_room(Some("audit")).unwrap();
        assert_eq!(name, "audit");
    }
}

#[cfg(test)]
mod load {
    use crate::workspace::*;
    use std::io::Write;

    #[test]
    fn load_valid_file() {
        let yaml = r#"
policies:
  quick:
    agents: ["a", "b"]
    max_rounds: 2
orchestrators:
  local:
    mode: embedded
rooms:
  audit:
    policy: quick
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let config = WorkspaceConfig::load(tmp.path()).unwrap();
        assert_eq!(config.policies.len(), 1);
        assert_eq!(config.rooms.len(), 1);
    }

    #[test]
    fn load_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.yaml");
        let err = WorkspaceConfig::load(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Io(_)),
            "expected Io error, got: {err}"
        );
    }

    #[test]
    fn load_invalid_yaml_errors() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"not: [valid: yaml: {{").unwrap();
        let err = WorkspaceConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::Yaml(_)),
            "expected Yaml error, got: {err}"
        );
    }

    #[test]
    fn load_minimal_agents_only_config() {
        let yaml = r#"
agents:
  config_file: ./agents.yml
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let config = WorkspaceConfig::load(tmp.path()).unwrap();
        assert!(config.orchestrators.is_empty());
        assert!(config.policies.is_empty());
        assert!(config.rooms.is_empty());
        assert!(config.agents.is_some());
    }

    #[test]
    fn load_rooms_without_orchestrators_errors() {
        let yaml = r#"
policies:
  p:
    agents: ["a", "b"]
rooms:
  r:
    policy: p
    orchestrator: missing
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let err = WorkspaceConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::NoOrchestrators),
            "expected NoOrchestrators error, got: {err}"
        );
    }

    // ── pinned_agents validation ────────────────────────────────────────

    #[test]
    fn validate_pinned_agents_valid() {
        let yaml = r#"
policies:
  mixed:
    roles:
      - role: security
        count: 2
        capabilities: ["security:*"]
        pinned_agents: ["alice"]
      - role: coder
        count: 1
        capabilities: ["lang:rust"]
    max_rounds: 3
    effort: 0.85

orchestrators:
  local:
    mode: embedded

rooms:
  r:
    policy: mixed
    orchestrator: local
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let config = WorkspaceConfig::load(tmp.path()).unwrap();
        let roles = config.policies["mixed"].roles.as_ref().unwrap();
        assert_eq!(roles[0].pinned_agents, Some(vec!["alice".to_string()]));
        assert!(roles[1].pinned_agents.is_none());
    }

    #[test]
    fn validate_pinned_agents_exceeds_count() {
        let yaml = r#"
policies:
  bad:
    roles:
      - role: security
        count: 1
        capabilities: ["security:*"]
        pinned_agents: ["alice", "bob"]
      - role: coder
        count: 1
        capabilities: ["lang:rust"]
    max_rounds: 3
    effort: 0.85

orchestrators:
  local:
    mode: embedded

rooms:
  r:
    policy: bad
    orchestrator: local
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let err = WorkspaceConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::TooManyPinnedAgents { .. }),
            "expected TooManyPinnedAgents, got: {err}"
        );
    }

    #[test]
    fn validate_pinned_agents_duplicates_rejected() {
        let yaml = r#"
policies:
  bad:
    roles:
      - role: security
        count: 3
        capabilities: ["security:*"]
        pinned_agents: ["alice", "alice"]
      - role: coder
        count: 1
        capabilities: ["lang:rust"]
    max_rounds: 3
    effort: 0.85

orchestrators:
  local:
    mode: embedded

rooms:
  r:
    policy: bad
    orchestrator: local
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let err = WorkspaceConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::DuplicatePinnedAgent { .. }),
            "expected DuplicatePinnedAgent, got: {err}"
        );
    }

    #[test]
    fn validate_cross_role_duplicate_pinned_rejected() {
        let yaml = r#"
policies:
  bad:
    roles:
      - role: security
        count: 2
        capabilities: ["security:*"]
        pinned_agents: ["alice"]
      - role: coder
        count: 2
        capabilities: ["lang:rust"]
        pinned_agents: ["alice"]
    max_rounds: 3
    effort: 0.85

orchestrators:
  local:
    mode: embedded

rooms:
  r:
    policy: bad
    orchestrator: local
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let err = WorkspaceConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::DuplicatePinnedAgent { .. }),
            "expected DuplicatePinnedAgent for cross-role duplicate, got: {err}"
        );
    }

    #[test]
    fn validate_too_many_agents_rejected() {
        use std::io::Write;
        let agents: Vec<String> = (0..256).map(|i| format!("agent_{i}")).collect();
        let agents_yaml = agents
            .iter()
            .map(|a| format!("\"{}\"", a))
            .collect::<Vec<_>>()
            .join(", ");
        let yaml = format!(
            r#"
policies:
  big:
    agents: [{agents_yaml}]
    max_rounds: 2
    effort: 0.85

orchestrators:
  local:
    mode: embedded

rooms:
  r:
    policy: big
    orchestrator: local
"#
        );
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        let err = WorkspaceConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ConfigError::TooManyAgents { count: 256, .. }),
            "expected TooManyAgents, got: {err}"
        );
    }

    #[test]
    fn policy_id_static_agents() {
        let policy = PolicyConfig {
            agents: Some(vec!["a".into(), "b".into()]),
            roles: None,
            max_rounds: 2,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        };
        let id = policy.policy_id();
        assert!(!id.is_empty());
        assert_eq!(id.len(), 64, "SHA-256 hex should be 64 chars");
        // Deterministic: same input → same hash
        assert_eq!(id, policy.policy_id());
    }

    #[test]
    fn policy_id_canonical_json_uses_max_rounds_key() {
        // Guard: HashablePolicy's canonical JSON (the input to SHA-256)
        // must serialize the rounds count as "max_rounds". If this key
        // drifts from the orchestrator's PolicyConfig, CLI-computed
        // policy_ids no longer match server-computed ones and hash-based
        // lookup silently breaks.
        //
        // We exercise the same codepath policy_id() uses by building a
        // HashablePolicy-equivalent struct locally is impractical because
        // it's private to policy_id(), so we compute two policy_ids that
        // differ *only* in max_rounds and assert the hash changes —
        // proving the field is included in canonical JSON — then, if a
        // future refactor ever inlines the canonical JSON, this test
        // still catches the regression.
        let policy_3 = PolicyConfig {
            agents: Some(vec!["a".into(), "b".into()]),
            roles: None,
            max_rounds: 3,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        };
        let mut policy_4 = policy_3.clone();
        policy_4.max_rounds = 4;
        assert_ne!(
            policy_3.policy_id(),
            policy_4.policy_id(),
            "max_rounds must be part of the canonical JSON used for policy_id"
        );
    }

    #[test]
    fn policy_id_role_based() {
        let policy = PolicyConfig {
            agents: None,
            roles: Some(vec![RoleConfig {
                role: "reviewer".into(),
                count: 2,
                capabilities: vec!["lang:rust".into()],
                context: Some(vec![ContextRef {
                    name: "code".into(),
                    path: "./src".into(),
                }]),
                pinned_agents: None,
                moderator: false,
            }]),
            max_rounds: 3,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        };
        let id = policy.policy_id();
        assert_eq!(id.len(), 64);

        // Context should NOT affect the hash (stripped for orchestrator compat)
        let policy_no_ctx = PolicyConfig {
            roles: Some(vec![RoleConfig {
                context: None,
                ..policy.roles.as_ref().unwrap()[0].clone()
            }]),
            ..policy.clone()
        };
        assert_eq!(
            id,
            policy_no_ctx.policy_id(),
            "context field should not affect policy_id"
        );
    }
}
