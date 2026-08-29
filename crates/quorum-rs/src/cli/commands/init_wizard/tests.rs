use super::*;
use crate::cli::remote::{AgentInfo, DiscoveredPolicy};
use crate::cli::workspace::ContextRef;

// ── Helper to build a simple config via the new API ─────────────────────────

fn simple_config(
    orch_name: &str,
    orch: OrchestratorConfig,
    policy_name: &str,
    policy: PolicyConfig,
    room_name: &str,
) -> WorkspaceConfig {
    let mut orchestrators = HashMap::new();
    orchestrators.insert(orch_name.to_string(), orch);
    let mut policies = HashMap::new();
    policies.insert(policy_name.to_string(), policy);
    let mut rooms = HashMap::new();
    rooms.insert(
        room_name.to_string(),
        RoomConfig {
            policy: policy_name.to_string(),
            orchestrator: Some(orch_name.to_string()),
        },
    );
    build_config(
        orchestrators,
        policies,
        rooms,
        Some(room_name.to_string()),
        None,
        None,
    )
}

fn remote_orch(address: &str, token: &str) -> OrchestratorConfig {
    OrchestratorConfig {
        mode: Some(OrchestratorMode::Remote),
        address: Some(address.into()),
        token: Some(token.into()),
        nats_url: None,
        config_file: None,
    }
}

fn static_policy(agents: &[&str]) -> PolicyConfig {
    PolicyConfig {
        variables: None,
        agents: Some(agents.iter().map(|s| s.to_string()).collect()),
        roles: None,
        max_rounds: 2,
        effort: 0.85,
        sla: None,
        capabilities: None,
        tags: None,
        mode: Default::default(),
    }
}

// ── sanitize_name ───────────────────────────────────────────────────────────

#[test]
fn sanitize_normal_slug() {
    assert_eq!(sanitize_name("my-orch"), Some("my-orch".into()));
}

#[test]
fn sanitize_with_spaces_and_caps() {
    assert_eq!(sanitize_name("  My Orch  "), Some("my_orch".into()));
}

#[test]
fn sanitize_special_chars() {
    assert_eq!(sanitize_name("foo/bar.baz"), Some("foo_bar_baz".into()));
}

#[test]
fn sanitize_empty() {
    assert_eq!(sanitize_name(""), None);
}

#[test]
fn sanitize_only_special() {
    assert_eq!(sanitize_name("///"), None);
}

#[test]
fn sanitize_underscores_stripped() {
    assert_eq!(sanitize_name("__my__name__"), Some("my_name".into()));
}

#[test]
fn sanitize_consecutive_underscores_collapsed() {
    assert_eq!(sanitize_name("foo///bar"), Some("foo_bar".into()));
    assert_eq!(sanitize_name("a  b  c"), Some("a_b_c".into()));
}

#[test]
fn sanitize_unicode() {
    assert_eq!(sanitize_name("café"), Some("café".into()));
}

// ── render_yaml ─────────────────────────────────────────────────────────────

#[test]
fn render_yaml_roundtrips() {
    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "${TOKEN}"),
        "review",
        static_policy(&["alice", "bob"]),
        "main",
    );

    let yaml = render_yaml(&config).expect("should serialize");
    assert!(yaml.contains("remote"), "yaml:\n{yaml}");
    assert!(yaml.contains("https://example.com"), "yaml:\n{yaml}");
    assert!(yaml.contains("${TOKEN}"), "yaml:\n{yaml}");
    assert!(yaml.contains("alice"), "yaml:\n{yaml}");
    assert!(yaml.contains("bob"), "yaml:\n{yaml}");
    assert!(yaml.contains("review"), "yaml:\n{yaml}");
    assert!(yaml.contains("main"), "yaml:\n{yaml}");

    // Should be parseable back
    let parsed: WorkspaceConfig = serde_yaml::from_str(&yaml).expect("should parse");
    assert_eq!(parsed.default_room, Some("main".into()));
    assert!(parsed.orchestrators.contains_key("remote"));
    assert!(parsed.policies.contains_key("review"));
    assert!(parsed.rooms.contains_key("main"));
}

// ── build_config ────────────────────────────────────────────────────────────

#[test]
fn build_config_wires_room_correctly() {
    let config = simple_config(
        "prod",
        remote_orch("https://api.peeramid.xyz", "secret"),
        "policy1",
        static_policy(&["a", "b"]),
        "room1",
    );

    assert_eq!(config.default_room, Some("room1".into()));
    let room = config.rooms.get("room1").expect("room should exist");
    assert_eq!(room.policy, "policy1");
    assert_eq!(room.orchestrator, Some("prod".into()));
}

#[test]
fn build_config_validates_ok() {
    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        static_policy(&["a", "b"]),
        "main",
    );

    assert!(
        config.validate().is_ok(),
        "generated config should pass validation"
    );
}

#[test]
fn build_config_with_sla() {
    let mut policy = static_policy(&["a", "b"]);
    policy.sla = Some(PolicySla {
        job_timeout_secs: 600,
        response_sla_secs: None,
        max_tokens: None,
    });

    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        policy,
        "main",
    );

    let p = config.policies.get("review").unwrap();
    assert!(p.sla.is_some());
    assert_eq!(p.sla.as_ref().unwrap().job_timeout_secs, 600);
}

#[test]
fn build_config_with_full_sla() {
    let mut policy = static_policy(&["a", "b"]);
    policy.sla = Some(PolicySla {
        job_timeout_secs: 600,
        response_sla_secs: Some(120),
        max_tokens: Some(4096),
    });

    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        policy,
        "main",
    );

    let sla = config.policies.get("review").unwrap().sla.as_ref().unwrap();
    assert_eq!(sla.job_timeout_secs, 600);
    assert_eq!(sla.response_sla_secs, Some(120));
    assert_eq!(sla.max_tokens, Some(4096));
    assert!(config.validate().is_ok());
}

// ── multiple orchestrators ──────────────────────────────────────────────────

#[test]
fn build_config_multiple_orchestrators() {
    let mut orchestrators = HashMap::new();
    orchestrators.insert(
        "remote".into(),
        remote_orch("https://api.example.com", "tok1"),
    );
    orchestrators.insert(
        "local".into(),
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Embedded),
            address: None,
            token: None,
            nats_url: Some("nats://localhost:4222".into()),
            config_file: Some("./config/orchestrator.yml".into()),
        },
    );

    let mut policies = HashMap::new();
    policies.insert("fast".into(), static_policy(&["a", "b"]));
    policies.insert("thorough".into(), static_policy(&["a", "b", "c"]));

    let mut rooms = HashMap::new();
    rooms.insert(
        "quick".into(),
        RoomConfig {
            policy: "fast".into(),
            orchestrator: Some("remote".into()),
        },
    );
    rooms.insert(
        "deep".into(),
        RoomConfig {
            policy: "thorough".into(),
            orchestrator: Some("local".into()),
        },
    );

    let config = build_config(
        orchestrators,
        policies,
        rooms,
        Some("quick".into()),
        None,
        None,
    );

    assert_eq!(config.orchestrators.len(), 2);
    assert_eq!(config.policies.len(), 2);
    assert_eq!(config.rooms.len(), 2);
    assert_eq!(config.default_room, Some("quick".into()));
    assert!(
        config.validate().is_ok(),
        "multi-orch config should validate"
    );
}

#[test]
fn build_config_no_default_room() {
    let config = build_config(
        HashMap::from([("r".into(), remote_orch("https://x.com", "t"))]),
        HashMap::from([("p".into(), static_policy(&["a", "b"]))]),
        HashMap::from([(
            "room".into(),
            RoomConfig {
                policy: "p".into(),
                orchestrator: Some("r".into()),
            },
        )]),
        None,
        None,
        None,
    );
    assert_eq!(config.default_room, None);
    assert!(config.validate().is_ok());
}

#[test]
fn build_config_with_agent_config_file() {
    let config = simple_config(
        "r",
        remote_orch("https://x.com", "t"),
        "p",
        static_policy(&["a", "b"]),
        "room",
    );
    assert!(config.agents.is_none(), "simple_config has no agents");

    let config_with_agents = build_config(
        HashMap::from([("r".into(), remote_orch("https://x.com", "t"))]),
        HashMap::from([("p".into(), static_policy(&["a", "b"]))]),
        HashMap::from([(
            "room".into(),
            RoomConfig {
                policy: "p".into(),
                orchestrator: Some("r".into()),
            },
        )]),
        Some("room".into()),
        Some("config/agent.yml".into()),
        None,
    );
    assert!(config_with_agents.agents.is_some());
    assert_eq!(
        config_with_agents.agents.as_ref().unwrap().config_file,
        "config/agent.yml"
    );
    assert!(
        config_with_agents
            .agents
            .as_ref()
            .unwrap()
            .dashboard_port
            .is_none()
    );
    assert!(config_with_agents.validate().is_ok());
}

#[test]
fn build_config_with_dashboard_port() {
    let config = build_config(
        HashMap::from([("r".into(), remote_orch("https://x.com", "t"))]),
        HashMap::from([("p".into(), static_policy(&["a", "b"]))]),
        HashMap::from([(
            "room".into(),
            RoomConfig {
                policy: "p".into(),
                orchestrator: Some("r".into()),
            },
        )]),
        Some("room".into()),
        Some("config/agent.yml".into()),
        Some(8081),
    );
    let agents = config.agents.as_ref().unwrap();
    assert_eq!(agents.config_file, "config/agent.yml");
    assert_eq!(agents.dashboard_port, Some(8081));
    assert!(config.validate().is_ok());
}

#[test]
fn dashboard_port_serializes_to_yaml() {
    let config = build_config(
        HashMap::from([("r".into(), remote_orch("https://x.com", "t"))]),
        HashMap::from([("p".into(), static_policy(&["a", "b"]))]),
        HashMap::from([(
            "room".into(),
            RoomConfig {
                policy: "p".into(),
                orchestrator: Some("r".into()),
            },
        )]),
        Some("room".into()),
        Some("config/agent.yml".into()),
        Some(9090),
    );

    let yaml = render_yaml(&config).expect("should serialize");
    assert!(
        yaml.contains("dashboard_port: 9090"),
        "dashboard_port should appear in YAML:\n{yaml}"
    );
}

#[test]
fn dashboard_port_none_omitted_from_yaml() {
    let config = build_config(
        HashMap::from([("r".into(), remote_orch("https://x.com", "t"))]),
        HashMap::from([("p".into(), static_policy(&["a", "b"]))]),
        HashMap::from([(
            "room".into(),
            RoomConfig {
                policy: "p".into(),
                orchestrator: Some("r".into()),
            },
        )]),
        Some("room".into()),
        Some("config/agent.yml".into()),
        None,
    );

    let yaml = render_yaml(&config).expect("should serialize");
    assert!(
        !yaml.contains("dashboard_port"),
        "dashboard_port: None should be omitted:\n{yaml}"
    );
}

// ── agent_display_options ───────────────────────────────────────────────────

#[test]
fn agent_display_online_and_offline() {
    let agents = vec![
        AgentInfo {
            agent_id: "alice".into(),
            model_name: "gpt-4".into(),
            provider_id: "openai".into(),
            is_online: true,
            estimated_cost_per_round: None,
            capability_tags: vec![],
            ..Default::default()
        },
        AgentInfo {
            agent_id: "bob".into(),
            model_name: "claude-3".into(),
            provider_id: "anthropic".into(),
            is_online: false,
            estimated_cost_per_round: None,
            capability_tags: vec![],
            ..Default::default()
        },
    ];

    let opts = agent_display_options(&agents);
    assert_eq!(opts.len(), 2);
    assert!(opts[0].starts_with('●'), "online agent should use ●");
    assert!(opts[1].starts_with('○'), "offline agent should use ○");
    assert!(opts[0].contains("alice"));
    assert!(opts[1].contains("bob"));
}

// ── parse_selected_agents ───────────────────────────────────────────────────

#[test]
fn parse_agents_maps_display_to_ids() {
    let agents = vec![
        AgentInfo {
            agent_id: "alice".into(),
            model_name: "gpt-4".into(),
            provider_id: "openai".into(),
            is_online: true,
            estimated_cost_per_round: None,
            capability_tags: vec![],
            ..Default::default()
        },
        AgentInfo {
            agent_id: "bob".into(),
            model_name: "claude-3".into(),
            provider_id: "anthropic".into(),
            is_online: false,
            estimated_cost_per_round: None,
            capability_tags: vec![],
            ..Default::default()
        },
        AgentInfo {
            agent_id: "charlie".into(),
            model_name: "llama-3".into(),
            provider_id: "ollama".into(),
            is_online: true,
            estimated_cost_per_round: None,
            capability_tags: vec![],
            ..Default::default()
        },
    ];

    let display = agent_display_options(&agents);
    let selected = vec![display[0].clone(), display[2].clone()];
    let ids = parse_selected_agents(&selected, &agents);
    assert_eq!(ids, vec!["alice", "charlie"]);
}

#[test]
fn parse_agents_empty_selection() {
    let agents = vec![AgentInfo {
        agent_id: "x".into(),
        model_name: "m".into(),
        provider_id: "p".into(),
        is_online: true,
        estimated_cost_per_round: None,
        capability_tags: vec![],
        ..Default::default()
    }];

    let ids = parse_selected_agents(&[], &agents);
    assert!(ids.is_empty());
}

// ── build_config embedded orchestrator ──────────────────────────────────────

#[test]
fn build_config_embedded_orchestrator() {
    let config = simple_config(
        "local",
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Embedded),
            address: None,
            token: None,
            nats_url: Some("nats://localhost:4222".into()),
            config_file: Some("./config/orchestrator.yml".into()),
        },
        "dev-policy",
        PolicyConfig {
            variables: None,
            agents: Some(vec!["a".into(), "b".into()]),
            roles: None,
            max_rounds: 1,
            effort: 0.7,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        },
        "dev",
    );

    let orch = config.orchestrators.get("local").unwrap();
    assert_eq!(orch.mode, Some(OrchestratorMode::Embedded));
    assert!(orch.nats_url.is_some());
    assert!(orch.config_file.is_some());
}

// ── render_default_orchestrator_config (reused from nsed-cli-common) ────────

#[test]
fn render_default_orchestrator_config_contains_sections() {
    let content = crate::init::render_default_orchestrator_config(8080, 4222);

    // All major sections present
    assert!(content.contains("server:"), "missing server section");
    assert!(content.contains("port: 8080"), "missing port");
    assert!(content.contains("nats:"), "missing nats section");
    assert!(
        content.contains("nats://127.0.0.1:4222"),
        "missing nats url"
    );
    assert!(content.contains("auth:"), "missing auth section");
    assert!(
        content.contains("credentials:"),
        "missing credentials section"
    );
    assert!(content.contains("logging:"), "missing logging section");
    assert!(
        content.contains("# orchestrator:"),
        "missing orchestrator tunables"
    );
    assert!(content.contains("# nsed:"), "missing nsed protocol section");
}

#[test]
fn render_default_orchestrator_config_custom_ports() {
    let content = crate::init::render_default_orchestrator_config(9090, 5222);

    assert!(content.contains("port: 9090"), "custom API port");
    assert!(
        content.contains("nats://127.0.0.1:5222"),
        "custom NATS port"
    );
}

// ── policy with capabilities and tags ───────────────────────────────────────

#[test]
fn build_config_with_capabilities_and_tags() {
    let mut policy = static_policy(&["a", "b"]);
    policy.capabilities = Some(vec!["lang:rust".into(), "security:*".into()]);
    policy.tags = Some(vec!["domain:security".into(), "public".into()]);

    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        policy,
        "main",
    );

    let p = config.policies.get("review").unwrap();
    assert_eq!(
        p.capabilities.as_ref().unwrap(),
        &["lang:rust", "security:*"]
    );
    assert_eq!(p.tags.as_ref().unwrap(), &["domain:security", "public"]);
    assert!(config.validate().is_ok());
}

#[test]
fn render_yaml_includes_capabilities_and_tags() {
    let mut policy = static_policy(&["a", "b"]);
    policy.capabilities = Some(vec!["lang:rust".into()]);
    policy.tags = Some(vec!["public".into()]);

    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        policy,
        "main",
    );

    let yaml = render_yaml(&config).expect("should serialize");
    assert!(yaml.contains("lang:rust"), "yaml:\n{yaml}");
    assert!(yaml.contains("public"), "yaml:\n{yaml}");
}

// ── role-based policy ───────────────────────────────────────────────────────

#[test]
fn build_config_role_based_policy() {
    let policy = PolicyConfig {
        variables: None,
        agents: None,
        roles: Some(vec![
            RoleConfig {
                role: "analyst".into(),
                count: 1,
                capabilities: vec!["reasoning".into()],
                context: None,
                pinned_agents: None,
                moderator: false,
            },
            RoleConfig {
                role: "reviewer".into(),
                count: 1,
                capabilities: vec!["*".into()],
                context: None,
                pinned_agents: None,
                moderator: false,
            },
        ]),
        max_rounds: 3,
        effort: 0.85,
        sla: None,
        capabilities: None,
        tags: None,
        mode: Default::default(),
    };

    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        policy,
        "main",
    );

    assert!(
        config.validate().is_ok(),
        "role-based config should pass validation"
    );
    let p = config.policies.get("review").unwrap();
    assert!(p.agents.is_none());
    assert_eq!(p.roles.as_ref().unwrap().len(), 2);
}

#[test]
fn build_config_role_based_validates_min_agents() {
    let policy = PolicyConfig {
        variables: None,
        agents: None,
        roles: Some(vec![RoleConfig {
            role: "solo".into(),
            count: 1,
            capabilities: vec!["*".into()],
            context: None,
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

    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        policy,
        "main",
    );

    assert!(
        config.validate().is_err(),
        "single-role policy with count=1 should fail validation"
    );
}

#[test]
fn build_config_role_with_context() {
    let policy = PolicyConfig {
        variables: None,
        agents: None,
        roles: Some(vec![
            RoleConfig {
                role: "analyst".into(),
                count: 1,
                capabilities: vec!["reasoning".into()],
                context: Some(vec![ContextRef {
                    name: "spec".into(),
                    path: "docs/spec.md".into(),
                }]),
                pinned_agents: None,
                moderator: false,
            },
            RoleConfig {
                role: "coder".into(),
                count: 1,
                capabilities: vec!["lang:rust".into()],
                context: Some(vec![
                    ContextRef {
                        name: "src".into(),
                        path: "src/main.rs".into(),
                    },
                    ContextRef {
                        name: "tests".into(),
                        path: "tests/".into(),
                    },
                ]),
                pinned_agents: None,
                moderator: false,
            },
        ]),
        max_rounds: 2,
        effort: 0.8,
        sla: None,
        capabilities: None,
        tags: None,
        mode: Default::default(),
    };

    let config = simple_config(
        "remote",
        remote_orch("https://example.com", "tok"),
        "review",
        policy,
        "main",
    );

    assert!(
        config.validate().is_ok(),
        "role+context config should validate"
    );
    let roles = config
        .policies
        .get("review")
        .unwrap()
        .roles
        .as_ref()
        .unwrap();
    assert_eq!(roles[0].context.as_ref().unwrap().len(), 1);
    assert_eq!(roles[1].context.as_ref().unwrap().len(), 2);
    assert_eq!(roles[1].context.as_ref().unwrap()[0].name, "src");
}

// ── parse_comma_list ────────────────────────────────────────────────────────

#[test]
fn parse_comma_list_empty() {
    assert_eq!(parse_comma_list(""), None);
    assert_eq!(parse_comma_list("  ,  , "), None);
}

#[test]
fn parse_comma_list_values() {
    assert_eq!(
        parse_comma_list("lang:rust, security:*"),
        Some(vec!["lang:rust".into(), "security:*".into()])
    );
}

#[test]
fn parse_comma_list_single() {
    assert_eq!(parse_comma_list("general"), Some(vec!["general".into()]));
}

// ── created_agent_display + parse_selected_created ──────────────────────────

#[test]
fn created_agent_display_formats_correctly() {
    let agents = vec![
        AgentSummary {
            name: "DEFAULT".into(),
            capability_tags: vec!["general".into()],
            model_name: "gpt-4o".into(),
            provider_id: "openai".into(),
        },
        AgentSummary {
            name: "REASON".into(),
            capability_tags: vec!["reasoning".into()],
            model_name: "claude-3.5".into(),
            provider_id: "anthropic".into(),
        },
    ];

    let display = created_agent_display(&agents);
    assert_eq!(display.len(), 2);
    assert!(display[0].contains("DEFAULT"));
    assert!(display[0].contains("gpt-4o"));
    assert!(display[1].contains("REASON"));
    assert!(display[1].contains("claude-3.5"));
}

#[test]
fn parse_selected_created_maps_back() {
    let agents = vec![
        AgentSummary {
            name: "DEFAULT".into(),
            capability_tags: vec![],
            model_name: "gpt-4o".into(),
            provider_id: "openai".into(),
        },
        AgentSummary {
            name: "REASON".into(),
            capability_tags: vec![],
            model_name: "claude-3.5".into(),
            provider_id: "anthropic".into(),
        },
        AgentSummary {
            name: "CREATE".into(),
            capability_tags: vec![],
            model_name: "llama-3".into(),
            provider_id: "ollama".into(),
        },
    ];

    let display = created_agent_display(&agents);
    let selected = vec![display[0].clone(), display[2].clone()];
    let names = parse_selected_created(&selected, &agents);
    assert_eq!(names, vec!["DEFAULT", "CREATE"]);
}

// ── combined_agent_display + parse_combined_selection ────────────────────────

#[test]
fn combined_display_merges_created_and_discovered() {
    let created = vec![AgentSummary {
        name: "DEFAULT".into(),
        capability_tags: vec!["general".into()],
        model_name: "gpt-4o".into(),
        provider_id: "openai".into(),
    }];

    let discovered = vec![AgentInfo {
        agent_id: "remote-alpha".into(),
        model_name: "claude-3".into(),
        provider_id: "anthropic".into(),
        is_online: true,
        estimated_cost_per_round: None,
        capability_tags: vec!["security:owasp".into()],
        ..Default::default()
    }];

    let (display, ids) = combined_agent_display(&created, &discovered);
    assert_eq!(display.len(), 2);
    assert_eq!(ids.len(), 2);

    // Created agent uses ★ prefix
    assert!(display[0].starts_with('★'), "created should use ★");
    assert!(display[0].contains("DEFAULT"));
    assert!(display[0].contains("[general]"));

    // Discovered agent uses ● (online)
    assert!(
        display[1].starts_with('●'),
        "online discovered should use ●"
    );
    assert!(display[1].contains("remote-alpha"));
    assert!(display[1].contains("[security:owasp]"));

    assert_eq!(ids[0], "DEFAULT");
    assert_eq!(ids[1], "remote-alpha");
}

#[test]
fn combined_display_only_created() {
    let created = vec![
        AgentSummary {
            name: "A".into(),
            capability_tags: vec![],
            model_name: "m1".into(),
            provider_id: "p1".into(),
        },
        AgentSummary {
            name: "B".into(),
            capability_tags: vec![],
            model_name: "m2".into(),
            provider_id: "p2".into(),
        },
    ];

    let (display, ids) = combined_agent_display(&created, &[]);
    assert_eq!(display.len(), 2);
    assert_eq!(ids, vec!["A", "B"]);
}

#[test]
fn combined_display_only_discovered() {
    let discovered = vec![AgentInfo {
        agent_id: "x".into(),
        model_name: "m".into(),
        provider_id: "p".into(),
        is_online: false,
        estimated_cost_per_round: None,
        capability_tags: vec![],
        ..Default::default()
    }];

    let (display, ids) = combined_agent_display(&[], &discovered);
    assert_eq!(display.len(), 1);
    assert!(display[0].starts_with('○'), "offline should use ○");
    assert_eq!(ids[0], "x");
}

#[test]
fn parse_combined_selection_maps_back() {
    let created = vec![AgentSummary {
        name: "LOCAL".into(),
        capability_tags: vec![],
        model_name: "gpt-4o".into(),
        provider_id: "openai".into(),
    }];

    let discovered = vec![
        AgentInfo {
            agent_id: "remote-a".into(),
            model_name: "claude-3".into(),
            provider_id: "anthropic".into(),
            is_online: true,
            estimated_cost_per_round: None,
            capability_tags: vec![],
            ..Default::default()
        },
        AgentInfo {
            agent_id: "remote-b".into(),
            model_name: "llama-3".into(),
            provider_id: "ollama".into(),
            is_online: false,
            estimated_cost_per_round: None,
            capability_tags: vec![],
            ..Default::default()
        },
    ];

    let (display, ids) = combined_agent_display(&created, &discovered);
    // Select first (created) and third (discovered offline)
    let selected = vec![display[0].clone(), display[2].clone()];
    let names = parse_combined_selection(&selected, &display, &ids);
    assert_eq!(names, vec!["LOCAL", "remote-b"]);
}

#[test]
fn combined_display_shows_capability_tags() {
    let created = vec![AgentSummary {
        name: "REASON".into(),
        capability_tags: vec!["reasoning".into(), "lang:rust".into()],
        model_name: "m".into(),
        provider_id: "p".into(),
    }];

    let (display, _) = combined_agent_display(&created, &[]);
    assert!(
        display[0].contains("[reasoning, lang:rust]"),
        "should show capability tags: {}",
        display[0]
    );
}

// ── resolve_orchestrator_url ────────────────────────────────────────────────

#[test]
fn resolve_url_prefers_remote() {
    let mut orchestrators = HashMap::new();
    orchestrators.insert(
        "local".into(),
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Embedded),
            address: None,
            token: None,
            nats_url: Some("nats://localhost:4222".into()),
            config_file: None,
        },
    );
    orchestrators.insert(
        "remote".into(),
        remote_orch("https://api.example.com", "tok"),
    );

    let url = resolve_orchestrator_url(&orchestrators);
    assert_eq!(url, "https://api.example.com");
}

#[test]
fn resolve_url_falls_back_to_localhost() {
    let mut orchestrators = HashMap::new();
    orchestrators.insert(
        "local".into(),
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Embedded),
            address: None,
            token: None,
            nats_url: Some("nats://localhost:4222".into()),
            config_file: None,
        },
    );

    let url = resolve_orchestrator_url(&orchestrators);
    assert_eq!(url, "http://localhost:8080");
}

#[test]
fn resolve_url_embedded_with_address() {
    let mut orchestrators = HashMap::new();
    orchestrators.insert(
        "local".into(),
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Embedded),
            address: Some("http://localhost:9090".into()),
            token: None,
            nats_url: Some("nats://localhost:4222".into()),
            config_file: None,
        },
    );

    let url = resolve_orchestrator_url(&orchestrators);
    assert_eq!(url, "http://localhost:9090");
}

// ── render_yaml_redacted ────────────────────────────────────────────────────

#[test]
fn render_yaml_redacted_masks_literal_tokens() {
    let config = simple_config(
        "prod",
        remote_orch("https://api.example.com", "my-secret-bearer-token"),
        "review",
        static_policy(&["alice", "bob"]),
        "main",
    );

    let redacted = render_yaml_redacted(&config).expect("should serialize");
    assert!(
        !redacted.contains("my-secret-bearer-token"),
        "literal token should be redacted:\n{redacted}"
    );
    assert!(
        redacted.contains("<redacted>"),
        "should contain <redacted> placeholder:\n{redacted}"
    );
}

#[test]
fn render_yaml_redacted_preserves_env_ref_tokens() {
    let config = simple_config(
        "prod",
        remote_orch("https://api.example.com", "${NSED_TOKEN}"),
        "review",
        static_policy(&["alice", "bob"]),
        "main",
    );

    let redacted = render_yaml_redacted(&config).expect("should serialize");
    assert!(
        redacted.contains("${NSED_TOKEN}"),
        "env-ref token should NOT be redacted:\n{redacted}"
    );
    assert!(
        !redacted.contains("<redacted>"),
        "env-ref token should not be replaced:\n{redacted}"
    );
}

#[test]
fn render_yaml_writes_full_token() {
    let config = simple_config(
        "prod",
        remote_orch("https://api.example.com", "my-secret-bearer-token"),
        "review",
        static_policy(&["alice", "bob"]),
        "main",
    );

    let yaml = render_yaml(&config).expect("should serialize");
    assert!(
        yaml.contains("my-secret-bearer-token"),
        "render_yaml (for disk write) should include the literal token:\n{yaml}"
    );
}

// ── format_next_steps tests ─────────────────────────────────────────────────

#[test]
fn next_steps_without_remote_omits_hint() {
    let msg = super::format_next_steps(false);
    assert!(msg.contains("quorum validate"));
    assert!(msg.contains("quorum run"));
    assert!(msg.contains("quorum tui"));
    assert!(msg.contains("quorum serve"));
    assert!(
        !msg.contains("Remote orchestrator detected"),
        "should not show remote hint when no remote orchestrators"
    );
}

#[test]
fn next_steps_with_remote_includes_serve_hint() {
    let msg = super::format_next_steps(true);
    assert!(msg.contains("quorum validate"));
    assert!(msg.contains("quorum run"));
    assert!(msg.contains("quorum tui"));
    assert!(msg.contains("quorum serve"));
    assert!(
        msg.contains("Remote orchestrator detected"),
        "should show remote hint when remote orchestrator is present"
    );
    assert!(
        msg.contains("contributing agents"),
        "hint should explain quorum serve is for agent contribution, not dispatch"
    );
}

// ── persist_agent_creds — wizard creds-write boundary cases ─────────────

/// Helper: scope `$HOME` to a tempdir so `persist_agent_creds`
/// writes inside it. Uses a `Drop` guard so a panic inside `f()`
/// still restores the original HOME — without the guard, an
/// unrelated later test would inherit the tempdir's HOME and either
/// fail or (worse) write to whatever junk path matched. Caller is
/// responsible for `#[serial_test::serial(home)]` so concurrent
/// HOME-mutating tests don't race the guard.
fn with_home<R>(home: &std::path::Path, f: impl FnOnce() -> R) -> R {
    struct HomeGuard(Option<String>);
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: serialised across the test binary by the
            // caller's `#[serial_test::serial(home)]` attribute.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
    let _guard = HomeGuard(std::env::var("HOME").ok());
    // SAFETY: same as above.
    unsafe {
        std::env::set_var("HOME", home);
    }
    f()
}

#[test]
#[serial_test::serial(home)]
fn persist_agent_creds_writes_both_files_under_dot_nsed() {
    let tmp = tempfile::tempdir().unwrap();
    let (creds, seed) = with_home(tmp.path(), || {
        super::persist_agent_creds("creds-content", "seed-content").unwrap()
    });
    assert!(creds.ends_with(".nsed/agent.creds"));
    assert!(seed.ends_with(".nsed/agent.seed"));
    let creds_body = std::fs::read_to_string(&creds).unwrap();
    let seed_body = std::fs::read_to_string(&seed).unwrap();
    assert!(creds_body.starts_with("creds-content"));
    assert!(seed_body.starts_with("seed-content"));
    // Trailing newline appended when input didn't already end with one.
    assert!(creds_body.ends_with('\n'));
    assert!(seed_body.ends_with('\n'));
}

#[cfg(unix)]
#[test]
#[serial_test::serial(home)]
fn persist_agent_creds_mode_is_0600_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (creds, seed) = with_home(tmp.path(), || {
        super::persist_agent_creds("cc", "ss").unwrap()
    });
    for path in [creds, seed] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} must be mode 0600; got {mode:o}",
            path.display()
        );
    }
}

#[test]
#[serial_test::serial(home)]
fn persist_agent_creds_rotates_existing_creds_to_backup() {
    // Regression: previously the wizard REFUSED to overwrite,
    // which lost the freshly-minted token (orchestrator had
    // already burned the JTI). Now an existing file is rotated to
    // a timestamped `.bak-<ts>` sidecar so the new write always
    // succeeds AND the operator can recover the old identity from
    // the backup if needed.
    let tmp = tempfile::tempdir().unwrap();
    let nsed_dir = tmp.path().join(".nsed");
    std::fs::create_dir_all(&nsed_dir).unwrap();
    std::fs::write(nsed_dir.join("agent.creds"), b"pre-existing").unwrap();
    let (creds, seed) = with_home(tmp.path(), || {
        super::persist_agent_creds("new-cc", "new-ss").unwrap()
    });
    // New content written to canonical path. `write_secret_file`
    // appends a trailing newline if the input lacks one.
    assert_eq!(std::fs::read_to_string(&creds).unwrap(), "new-cc\n");
    assert_eq!(std::fs::read_to_string(&seed).unwrap(), "new-ss\n");
    // Old content preserved under a .bak-<ts> sidecar.
    let entries: Vec<_> = std::fs::read_dir(&nsed_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    let backup = entries
        .iter()
        .find(|n| n.starts_with("agent.creds.bak-"))
        .unwrap_or_else(|| panic!("no creds backup found in {entries:?}"));
    let backup_body = std::fs::read_to_string(nsed_dir.join(backup)).unwrap();
    assert_eq!(backup_body, "pre-existing");
}

#[test]
#[serial_test::serial(home)]
fn persist_agent_creds_rotates_existing_seed_to_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let nsed_dir = tmp.path().join(".nsed");
    std::fs::create_dir_all(&nsed_dir).unwrap();
    std::fs::write(nsed_dir.join("agent.seed"), b"pre-existing-seed").unwrap();
    let (_creds, seed) = with_home(tmp.path(), || {
        super::persist_agent_creds("new-cc", "new-ss").unwrap()
    });
    assert_eq!(std::fs::read_to_string(&seed).unwrap(), "new-ss\n");
    let entries: Vec<_> = std::fs::read_dir(&nsed_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    let backup = entries
        .iter()
        .find(|n| n.starts_with("agent.seed.bak-"))
        .unwrap_or_else(|| panic!("no seed backup found in {entries:?}"));
    let backup_body = std::fs::read_to_string(nsed_dir.join(backup)).unwrap();
    assert_eq!(backup_body, "pre-existing-seed");
}

// ── Remote-policy → local-room helpers ──────────────────────────────────────

fn discovered(id: &str, name: &str, tags: &[&str]) -> DiscoveredPolicy {
    DiscoveredPolicy {
        policy_id: id.to_string(),
        name: name.to_string(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn format_remote_policy_option_single_orch_drops_orch_prefix() {
    let p = discovered("pol.alpha", "Alpha", &["pl:acme"]);
    let label = super::format_remote_policy_option("remote", false, &p);
    assert!(
        label.starts_with("pol.alpha — Alpha"),
        "expected id+name first, got: {label}"
    );
    assert!(label.contains("[pl:acme]"), "tag hint missing: {label}");
    assert!(
        !label.contains("remote/"),
        "single-orch label must not be qualified, got: {label}"
    );
}

#[test]
fn format_remote_policy_option_multi_orch_qualifies_with_orch_name() {
    let p = discovered("pol.alpha", "Alpha", &[]);
    let label = super::format_remote_policy_option("east", true, &p);
    assert!(
        label.starts_with("east/pol.alpha — Alpha"),
        "expected `east/<id> — <name>` prefix, got: {label}"
    );
    // No tag hint when tags empty.
    assert!(
        !label.contains('['),
        "empty-tag label should have no [], got: {label}"
    );
}

#[test]
fn unique_room_name_uses_policy_id_when_free() {
    let rooms: HashMap<String, RoomConfig> = HashMap::new();
    let name = super::unique_room_name(&rooms, "remote", "alpha");
    assert_eq!(name, "alpha");
}

#[test]
fn unique_room_name_falls_back_to_qualified_on_clash() {
    let mut rooms: HashMap<String, RoomConfig> = HashMap::new();
    rooms.insert(
        "alpha".to_string(),
        RoomConfig {
            policy: "old".into(),
            orchestrator: None,
        },
    );
    let name = super::unique_room_name(&rooms, "east", "alpha");
    assert_eq!(name, "east__alpha");
}

#[test]
fn unique_room_name_appends_counter_on_double_clash() {
    let mut rooms: HashMap<String, RoomConfig> = HashMap::new();
    for n in ["alpha", "east__alpha"] {
        rooms.insert(
            n.to_string(),
            RoomConfig {
                policy: "x".into(),
                orchestrator: None,
            },
        );
    }
    let name = super::unique_room_name(&rooms, "east", "alpha");
    assert_eq!(name, "east__alpha_2");
}

// ── credential-file hardening ───────────────────────────────────────────────

/// On Unix, `write_orchestrator_config_file` must persist the
/// file with `0o600` perms. The orchestrator config holds the JWT
/// signing seed + provider API key references — anything that
/// reaches disk here is treated as secret-equivalent.
#[cfg(unix)]
#[test]
fn write_orchestrator_config_file_uses_0o600_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("orchestrator.yml");
    super::write_orchestrator_config_file(&path, "server:\n  port: 8080\n").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "orchestrator config must be owner-only readable; got {mode:o}"
    );
    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(body, "server:\n  port: 8080\n");
}

/// Overwriting an existing config also re-applies `0o600`. Catches
/// a future refactor that switches to `std::fs::write` for the
/// re-write branch.
#[cfg(unix)]
#[test]
fn write_orchestrator_config_file_truncates_existing_and_keeps_0o600() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("orchestrator.yml");
    super::write_orchestrator_config_file(&path, "first").unwrap();
    super::write_orchestrator_config_file(&path, "second").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
}

/// `warn_if_token_file_world_readable` is a warn-only side-effect
/// helper — locking that it stays warn-only (does not panic / return
/// an error) when the token file is over-permissive prevents a
/// future tightening from breaking legitimate single-user setups.
#[cfg(unix)]
#[test]
fn warn_if_token_file_world_readable_does_not_block_read() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("operator.token");
    std::fs::write(&path, "bearer-token").unwrap();
    // Make it world-readable.
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&path, perms).unwrap();
    // Must not panic; the read continues regardless.
    super::warn_if_token_file_world_readable(&path);
    assert!(path.exists());
}

#[test]
fn merge_unified_appends_fleet_and_drops_orchestrators_list() {
    let workspace = "orchestrators:\n  primary:\n    token: \"file:x\"\n";
    // A rendered agent.yml: orchestrators LIST + providers + agents.
    let fleet = "\
orchestrators:
  - id: \"primary\"
    bearer_token: \"x\"
# =============================================================================
# LLM Providers
# =============================================================================
providers:
  openai:
    type: openai
agents:
  - name: justindgx
";
    let merged = super::merge_unified(workspace, Some(fleet));
    assert!(
        merged.contains("orchestrators:\n  primary:"),
        "workspace map kept"
    );
    assert!(merged.contains("providers:"), "fleet providers appended");
    assert!(merged.contains("name: justindgx"), "fleet agents appended");
    assert!(
        !merged.contains("- id: \"primary\""),
        "fleet orchestrators LIST dropped"
    );
}

#[test]
fn merge_unified_none_fleet_is_workspace_only() {
    let workspace = "orchestrators:\n  primary:\n    token: \"file:x\"\n";
    assert_eq!(super::merge_unified(workspace, None), workspace);
}

#[test]
fn with_dashboard_port_appends_top_level_and_flows_to_fleet() {
    let ws = "orchestrators:\n  primary:\n    token: \"file:x\"\npolicies:\n  default:\n    agents: [a, b]\nrooms:\n  m:\n    policy: default\n    orchestrator: primary\ndefault_room: m\n";
    let yaml = super::with_dashboard_port(ws.to_string(), Some(8081));
    assert!(yaml.contains("dashboard_port: 8081"));
    let cfg: crate::cli::workspace::QuorumConfig =
        serde_yaml::from_str(&yaml).expect("parses as QuorumConfig");
    assert_eq!(cfg.dashboard_port, Some(8081));
    // The value `quorum serve` reads to start the dashboard.
    assert_eq!(cfg.to_fleet().dashboard_port, Some(8081));
}

#[test]
fn with_dashboard_port_none_is_noop() {
    assert_eq!(
        super::with_dashboard_port("x: 1\n".to_string(), None),
        "x: 1\n"
    );
}
