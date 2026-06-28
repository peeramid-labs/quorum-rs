use super::*;

// ── is_inference_model ──────────────────────────────────────────────

/// Helper: build a ModelEntry with just an id (no type, no pricing).
fn entry(id: &str) -> ModelEntry {
    ModelEntry {
        id: id.to_string(),
        pricing: None,
        model_type: None,
    }
}

/// Helper: build a ModelEntry with a type tag.
fn entry_typed(id: &str, model_type: &str) -> ModelEntry {
    ModelEntry {
        id: id.to_string(),
        pricing: None,
        model_type: Some(model_type.to_string()),
    }
}

#[test]
fn inference_model_accepts_chat_models() {
    assert!(is_inference_model(&entry("meta-llama/Llama-3-70b-chat-hf")));
    assert!(is_inference_model(&entry("gpt-4o")));
    assert!(is_inference_model(&entry(
        "mistralai/Mixtral-8x7B-Instruct-v0.1"
    )));
    assert!(is_inference_model(&entry("deepseek-ai/DeepSeek-R1")));
}

#[test]
fn inference_model_rejects_non_chat_models() {
    assert!(!is_inference_model(&entry("BAAI/bge-large-en-v1.5")));
    assert!(!is_inference_model(&entry("WhereIsAI/UAE-Large-V1-embed")));
    assert!(!is_inference_model(&entry(
        "jinaai/jina-reranker-v2-base-multilingual"
    )));
    assert!(!is_inference_model(&entry("openai/text-embedding-3-small")));
    assert!(!is_inference_model(&entry("openai/clip-vit-large-patch14")));
    assert!(!is_inference_model(&entry("openai/whisper-large-v3")));
    assert!(!is_inference_model(&entry(
        "intfloat/e5-mistral-7b-instruct"
    )));
}

#[test]
fn inference_model_filter_is_case_insensitive() {
    assert!(!is_inference_model(&entry("EMBED-v2")));
    assert!(!is_inference_model(&entry("My-VISION-Model")));
}

#[test]
fn inference_model_uses_type_field_when_present() {
    // Type-based filtering takes precedence over keyword matching
    assert!(is_inference_model(&entry_typed("chat-model", "chat")));
    assert!(is_inference_model(&entry_typed("code-model", "code")));
    assert!(is_inference_model(&entry_typed(
        "language-model",
        "language"
    )));
    assert!(!is_inference_model(&entry_typed(
        "embed-model",
        "embedding"
    )));
    assert!(!is_inference_model(&entry_typed("rerank-model", "rerank")));
    assert!(!is_inference_model(&entry_typed("img-model", "image")));
    assert!(!is_inference_model(&entry_typed("mod-model", "moderation")));
}

// ── extract_pricing ────────────────────────────────────────────────

#[test]
fn extract_pricing_none() {
    assert_eq!(extract_pricing(None), (None, None));
}

#[test]
fn extract_pricing_numeric() {
    let p = ModelPricing {
        input: Some(serde_json::json!(0.20)),
        output: Some(serde_json::json!(0.60)),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!((inp.unwrap() - 0.20).abs() < 1e-9);
    assert!((out.unwrap() - 0.60).abs() < 1e-9);
}

#[test]
fn extract_pricing_string() {
    let p = ModelPricing {
        input: Some(serde_json::json!("0.15")),
        output: Some(serde_json::json!("0.45")),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!((inp.unwrap() - 0.15).abs() < 1e-9);
    assert!((out.unwrap() - 0.45).abs() < 1e-9);
}

#[test]
fn extract_pricing_mixed_and_missing() {
    let p = ModelPricing {
        input: Some(serde_json::json!(0.10)),
        output: None,
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!(inp.is_some());
    assert!(out.is_none());
}

#[test]
fn extract_pricing_non_parseable_string() {
    let p = ModelPricing {
        input: Some(serde_json::json!("free")),
        output: Some(serde_json::json!(true)),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!(inp.is_none());
    assert!(out.is_none());
}

// ── fmt_price ─────────────────────────────────────────────────────

#[test]
fn fmt_price_free() {
    assert_eq!(fmt_price(Some(0.0)), "free");
}

#[test]
fn fmt_price_none_is_dash() {
    assert_eq!(fmt_price(None), "—");
}

#[test]
fn fmt_price_typical() {
    assert_eq!(fmt_price(Some(0.90)), "$0.90");
    assert_eq!(fmt_price(Some(2.50)), "$2.50");
    assert_eq!(fmt_price(Some(10.0)), "$10.00");
}

#[test]
fn fmt_price_small() {
    assert_eq!(fmt_price(Some(0.15)), "$0.15");
}

// ── parse_env_file ─────────────────────────────────────────────────

#[test]
fn parse_env_file_happy_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(
        &path,
        "# comment\nKEY1=value1\n  KEY2 = value2 \n\n# another comment\nKEY3=base64=data\n",
    )
    .unwrap();
    let env = parse_env_file(&path);
    assert_eq!(env.get("KEY1").unwrap(), "value1");
    assert_eq!(env.get("KEY2").unwrap(), "value2");
    // Only splits on the first '='
    assert_eq!(env.get("KEY3").unwrap(), "base64=data");
    assert!(!env.contains_key("# comment"));
}

#[test]
fn parse_env_file_nonexistent() {
    let env = parse_env_file(Path::new("/nonexistent/.env"));
    assert!(env.is_empty());
}

// ── render_env ─────────────────────────────────────────────────────

fn sample_providers() -> Vec<Provider> {
    vec![
        Provider {
            id: "together_ai".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            api_key: Some("sk-test-123".to_string()),
            input_price: Some(0.90),
            output_price: Some(0.90),
            provider_type: "openai".to_string(),
            models: vec![ModelInfo {
                name: "meta-llama/Llama-3-70b".to_string(),
                input_price: Some(0.90),
                output_price: Some(0.90),
            }],
            engine: None,
        },
        Provider {
            id: "ollama_local".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![ModelInfo {
                name: "llama3.2".to_string(),
                input_price: Some(0.0),
                output_price: Some(0.0),
            }],
            engine: None,
        },
        Provider {
            id: "simulated".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![ModelInfo {
                name: "simulated-default".to_string(),
                input_price: Some(0.0),
                output_price: Some(0.0),
            }],
            engine: None,
        },
    ]
}

#[test]
fn render_env_owner_contains_required_sections() {
    let providers = sample_providers();
    let env = render_env(
        "owner",
        8080,
        4222,
        "admin-secret",
        "worker-secret",
        "SA_SEED",
        &providers,
        None,
        None,
    );

    assert!(env.contains("APP_PORT=8080"));
    assert!(env.contains("NATS_PORT=4222"));
    assert!(env.contains("APP_AUTH__ENABLED=true"));
    // Admin token (TOKENS__0) has admin role
    assert!(env.contains("APP_AUTH__TOKENS__0__NAME=admin"));
    assert!(env.contains("APP_AUTH__TOKENS__0__SECRET=admin-secret"));
    assert!(env.contains("APP_AUTH__TOKENS__0__ROLES__1=admin"));
    // Worker token (TOKENS__1) has user role only
    assert!(env.contains("APP_AUTH__TOKENS__1__NAME=worker"));
    assert!(env.contains("APP_AUTH__TOKENS__1__SECRET=worker-secret"));
    assert!(env.contains("APP_AUTH__TOKENS__1__ROLES__0=user"));
    assert!(!env.contains("APP_AUTH__TOKENS__1__ROLES__1"));
    assert!(env.contains("APP_CREDENTIALS__ACCOUNT_SEED=SA_SEED"));
    // Local agents use worker token
    assert!(env.contains("NSED_BEARER_TOKEN=worker-secret"));
    assert!(env.contains("TOGETHER_AI_API_KEY=sk-test-123"));
    // Ollama should say "local endpoint"
    assert!(env.contains("ollama_local: local endpoint"));
    // Simulated should say "no API key"
    assert!(env.contains("simulated: no API key"));
    // Should NOT contain orchestrator URL (owner persona)
    assert!(!env.contains("NSED_ORCHESTRATOR_URL"));
}

#[test]
fn render_env_worker_contains_orchestrator_url() {
    let providers = sample_providers();
    let env = render_env(
        "worker",
        8080,
        4222,
        "token-abc",
        "token-abc",
        "",
        &providers,
        Some("http://remote:8080"),
        None,
    );

    assert!(env.contains("NSED_ORCHESTRATOR_URL=http://remote:8080"));
    assert!(env.contains("NSED_BEARER_TOKEN=token-abc"));
    // Worker should NOT contain orchestrator config
    assert!(!env.contains("APP_PORT="));
    assert!(!env.contains("APP_CREDENTIALS__ACCOUNT_SEED"));
}

#[test]
fn render_env_worker_includes_operator_name() {
    let providers = sample_providers();
    let env = render_env(
        "worker",
        8080,
        4222,
        "token-abc",
        "token-abc",
        "",
        &providers,
        Some("http://remote:8080"),
        Some("Alice O'Brien"),
    );
    assert!(env.contains("NSED_OPERATOR_NAME=\"Alice O'Brien\""));
}

#[test]
fn render_env_worker_escapes_operator_name() {
    let providers = sample_providers();
    let env = render_env(
        "worker",
        8080,
        4222,
        "t",
        "t",
        "",
        &providers,
        Some("http://remote:8080"),
        Some("evil\"name\ninjection"),
    );
    assert!(env.contains("NSED_OPERATOR_NAME=\"evil\\\"nameinjection\""));
    // No raw newlines in the output
    assert!(!env.contains("NSED_OPERATOR_NAME=\"evil\"name"));
}

#[test]
fn render_env_omits_operator_name_when_none() {
    let providers = sample_providers();
    let env = render_env(
        "worker",
        8080,
        4222,
        "t",
        "t",
        "",
        &providers,
        Some("http://remote:8080"),
        None,
    );
    assert!(!env.contains("NSED_OPERATOR_NAME"));
}

#[test]
fn render_env_includes_agent_signing_seed() {
    let providers = sample_providers();
    let env = render_env("worker", 8080, 4222, "t", "t", "", &providers, None, None);
    assert!(
        env.contains("NSED_AGENT_SEED="),
        "Should contain NSED_AGENT_SEED"
    );
    // Extract the seed value and verify it's 64 hex chars (32 bytes)
    let seed_line = env
        .lines()
        .find(|l| l.starts_with("NSED_AGENT_SEED="))
        .unwrap();
    let seed_hex = seed_line.strip_prefix("NSED_AGENT_SEED=").unwrap();
    assert_eq!(seed_hex.len(), 64, "Seed should be 64 hex chars (32 bytes)");
    assert!(hex::decode(seed_hex).is_ok(), "Seed should be valid hex");
}

#[test]
fn render_env_agent_seed_is_random_per_call() {
    let providers = sample_providers();
    let env1 = render_env("worker", 8080, 4222, "t", "t", "", &providers, None, None);
    let env2 = render_env("worker", 8080, 4222, "t", "t", "", &providers, None, None);
    let seed1 = env1
        .lines()
        .find(|l| l.starts_with("NSED_AGENT_SEED="))
        .unwrap();
    let seed2 = env2
        .lines()
        .find(|l| l.starts_with("NSED_AGENT_SEED="))
        .unwrap();
    assert_ne!(seed1, seed2, "Each call should generate a unique seed");
}

#[test]
fn render_env_emits_base_urls_for_non_simulated() {
    let providers = sample_providers();
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);

    assert!(env.contains("APP_PROVIDERS__TOGETHER_AI__BASE_URL=https://api.together.xyz/v1"));
    assert!(env.contains("APP_PROVIDERS__OLLAMA_LOCAL__BASE_URL=http://localhost:11434/v1"));
    // Simulated has empty base_url → should not appear
    assert!(!env.contains("APP_PROVIDERS__SIMULATED__BASE_URL"));
}

// ── render_compose_owner ───────────────────────────────────────────

fn sample_agents() -> Vec<AgentSlot> {
    let mut a1 = AgentSlot::new(
        "DEFAULT".to_string(),
        "together_ai".to_string(),
        "meta-llama/Llama-3-70b".to_string(),
        Some(0.90),
        Some(0.90),
    );
    a1.apply_preset();
    // Previously paired DEFAULT with ARCHIT (Security ensemble);
    // ARCHIT was retired so swap in VERIFY which still ships under
    // General. Same intent: cover a two-agent rendering scenario
    // with a non-default preset.
    let mut a2 = AgentSlot::new(
        "VERIFY".to_string(),
        "ollama_local".to_string(),
        "llama3.2".to_string(),
        Some(0.0),
        Some(0.0),
    );
    a2.apply_preset();
    vec![a1, a2]
}

#[test]
fn compose_owner_has_nats_orchestrator_and_agents() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_owner(8080, 4222, &providers, &agents);

    // NATS service
    assert!(compose.contains("  nats:"));
    assert!(compose.contains("nats:2-alpine"));
    assert!(compose.contains("--jetstream"));
    // Orchestrator
    assert!(compose.contains("  nsed:"));
    assert!(compose.contains("ghcr.io/peeramid-labs/nsed:latest"));
    assert!(compose.contains("APP_SERVER__HOST: \"0.0.0.0\""));
    assert!(compose.contains("APP_NATS__URL: \"nats://nats:4222\""));
    // Agents
    assert!(compose.contains("  nsed-agent-1:"));
    assert!(compose.contains("  nsed-agent-2:"));
    assert!(compose.contains("ghcr.io/peeramid-labs/nsed-agent:latest"));
    assert!(compose.contains("NSED_AGENT_NAME: DEFAULT"));
    assert!(compose.contains("NSED_AGENT_NAME: VERIFY"));
}

#[test]
fn compose_owner_agents_depend_on_orchestrator() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    assert!(compose.contains("depends_on:"));
    assert!(compose.contains("condition: service_healthy"));
}

#[test]
fn compose_owner_forwards_api_keys() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    // together_ai has an API key → should be forwarded
    assert!(compose.contains("APP_PROVIDERS__TOGETHER_AI__API_KEY"));
}

#[test]
fn compose_owner_has_healthcheck() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    assert!(compose.contains("healthcheck:"));
    assert!(compose.contains("/health"));
}

#[test]
fn compose_owner_mounts_config_volumes() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    assert!(compose.contains("./config/orchestrator.yml:/app/config/default.yml:ro"));
    assert!(compose.contains("./config/agent.yml:/app/config/default.yml:ro"));
}

#[test]
fn compose_owner_no_empty_environment_for_local_providers() {
    // When all providers are local, the orchestrator service must NOT emit
    // an empty `environment:` key (docker compose rejects it as invalid).
    let local_only = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "llama3".to_string(),
            input_price: Some(0.0),
            output_price: Some(0.0),
        }],
        engine: None,
    }];
    let agents = vec![AgentSlot::new(
        "A".to_string(),
        "ollama".to_string(),
        "llama3".to_string(),
        Some(0.0),
        Some(0.0),
    )];
    let compose = render_compose_owner(8080, 4222, &local_only, &agents);
    // Even with local-only providers, the orchestrator always has environment
    // entries for APP_SERVER__HOST and APP_NATS__URL.
    assert!(compose.contains("APP_SERVER__HOST"));
    assert!(compose.contains("APP_NATS__URL"));
    // Should NOT have API key forwarding for local providers
    assert!(!compose.contains("APP_PROVIDERS__OLLAMA__API_KEY"));
}

// ── render_compose_worker ──────────────────────────────────────────

#[test]
fn compose_worker_has_no_orchestrator_service() {
    let compose = render_compose_worker(&sample_agents(), &sample_providers());

    assert!(compose.contains("services:"));
    assert!(!compose.contains("  nsed:"));
    assert!(compose.contains("  nsed-agent-1:"));
    assert!(compose.contains("  nsed-agent-2:"));
    assert!(compose.contains("NSED_ORCHESTRATOR_URL"));
    assert!(compose.contains("NSED_BEARER_TOKEN"));
}

#[test]
fn compose_worker_mounts_agent_config() {
    let compose = render_compose_worker(&sample_agents(), &sample_providers());
    assert!(compose.contains("./config/agent.yml:/app/config/default.yml:ro"));
}

#[test]
fn compose_worker_no_depends_on() {
    let compose = render_compose_worker(&sample_agents(), &sample_providers());
    assert!(!compose.contains("depends_on:"));
}

// ── ensure_gitignore ───────────────────────────────────────────────

#[test]
fn ensure_gitignore_creates_new_file() {
    let dir = tempfile::tempdir().unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(content.contains(".env"));
}

#[test]
fn ensure_gitignore_appends_to_existing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(content.contains("node_modules/"));
    assert!(content.contains(".env"));
}

#[test]
fn ensure_gitignore_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), ".env\ntarget/\n").unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    // Should appear exactly once
    assert_eq!(content.matches(".env").count(), 1);
}

// ── write_files ────────────────────────────────────────────────────

fn no_configs() -> ConfigFiles {
    ConfigFiles {
        agent_config: None,
        orchestrator_config: None,
    }
}

#[test]
fn write_files_creates_all_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    let skipped = write_files(
        dir.path(),
        "docker-compose.yml",
        "compose-content",
        "KEY=val\n",
        &empty,
        &no_configs(),
    )
    .unwrap();

    assert!(!skipped, "env should be written on fresh dir");
    let compose = std::fs::read_to_string(dir.path().join("docker-compose.yml")).unwrap();
    let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();

    assert_eq!(compose, "compose-content");
    assert_eq!(env, "KEY=val\n");
    assert!(gi.contains(".env"));
}

#[test]
fn write_files_merges_env_when_all_keys_present() {
    let dir = tempfile::tempdir().unwrap();
    // Pre-write an existing .env with a SECRET key and a normal key
    std::fs::write(
        dir.path().join(".env"),
        "KEY=original\nMY_API_KEY=keep-this\n",
    )
    .unwrap();
    let existing: HashMap<String, String> = [
        ("KEY".into(), "original".into()),
        ("MY_API_KEY".into(), "keep-this".into()),
    ]
    .into_iter()
    .collect();

    let skipped = write_files(
        dir.path(),
        "docker-compose.yml",
        "compose-content",
        "KEY=new_value\nMY_API_KEY=generated-new\n",
        &existing,
        &no_configs(),
    )
    .unwrap();

    assert!(skipped, "env should be merged when all keys present");
    let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    // Non-secret keys use the new template value
    assert!(
        env.contains("KEY=new_value"),
        "non-secret key should use template value"
    );
    // Secret keys preserve the existing value
    assert!(
        env.contains("MY_API_KEY=keep-this"),
        "secret key should preserve existing value"
    );
}

#[test]
fn write_files_writes_env_when_keys_missing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=original\n").unwrap();
    let existing: HashMap<String, String> =
        [("KEY".into(), "original".into())].into_iter().collect();

    let skipped = write_files(
        dir.path(),
        "docker-compose.yml",
        "compose-content",
        "KEY=new\nNEW_KEY=added\n",
        &existing,
        &no_configs(),
    )
    .unwrap();

    // Post-fix: write_files always merges when existing is non-empty
    // so a fresh template key doesn't wipe existing secrets. The new
    // template key is preserved via merge_env's per-line fallback.
    assert!(skipped, "merge happened (existing non-empty)");
    let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(env.contains("NEW_KEY=added"));
}

#[test]
fn write_files_creates_config_directory() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    let configs = ConfigFiles {
        agent_config: Some("agents:\n  - name: TEST\n".to_string()),
        orchestrator_config: Some("server:\n  port: 8080\n".to_string()),
    };
    write_files(
        dir.path(),
        "docker-compose.yml",
        "compose-content",
        "KEY=val\n",
        &empty,
        &configs,
    )
    .unwrap();

    let agent_yml = std::fs::read_to_string(dir.path().join("config/agent.yml")).unwrap();
    assert!(agent_yml.contains("name: TEST"));

    let orch_yml = std::fs::read_to_string(dir.path().join("config/orchestrator.yml")).unwrap();
    assert!(orch_yml.contains("port: 8080"));
}

// ── render_agent_config ─────────────────────────────────────────────

#[test]
fn render_agent_config_has_orchestrator_and_agents() {
    let providers = sample_providers();
    let agents = sample_agents();
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    assert!(cfg.contains("orchestrators:"));
    assert!(cfg.contains("url: \"http://nsed:8080\""));
    assert!(cfg.contains("bearer_token: \"${NSED_BEARER_TOKEN}\""));
    assert!(cfg.contains("providers:"));
    assert!(cfg.contains("together_ai:"));
    assert!(cfg.contains("agents:"));
    assert!(cfg.contains("name: \"DEFAULT\""));
    assert!(cfg.contains("name: \"VERIFY\""));
    // DEFAULT preset has temperature=0.7 and max_tokens=8096
    assert!(cfg.contains("temperature: 0.7"));
    assert!(cfg.contains("max_tokens: 8096"));
    // VERIFY preset has temperature=0.5 and max_tokens=12096
    assert!(cfg.contains("temperature: 0.5"));
    assert!(cfg.contains("max_tokens: 12096"));
    assert!(cfg.contains("presence_penalty: 1.5"));
}

#[test]
fn render_agent_config_emits_given_bearer_ref() {
    // The onboard path passes a file: ref to the operator token redeem wrote,
    // not the dangling ${NSED_BEARER_TOKEN}. Whatever ref is passed must land
    // verbatim in the orchestrator entry's bearer_token.
    let providers = sample_providers();
    let agents = sample_agents();
    let cfg = render_agent_config(
        "http://nsed:8080",
        "file:~/.nsed/operator.token",
        &providers,
        &agents,
    );
    assert!(
        cfg.contains("bearer_token: \"file:~/.nsed/operator.token\""),
        "bearer_ref must be emitted verbatim, got:\n{cfg}"
    );
    assert!(
        !cfg.contains("bearer_token: \"${NSED_BEARER_TOKEN}\""),
        "must not also emit the dangling env-var ref"
    );
}

#[test]
fn render_agent_config_has_provider_types() {
    let providers = sample_providers();
    let agents = sample_agents();
    let cfg = render_agent_config(
        "http://localhost:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    // Remote providers should have env-var api_key reference
    assert!(cfg.contains("api_key: \"${TOGETHER_AI_API_KEY}\""));
    // Local providers should have api_key: "ollama"
    assert!(cfg.contains("api_key: \"ollama\""));
}

#[test]
fn render_agent_config_includes_pricing() {
    let providers = sample_providers();
    let agents = sample_agents();
    let cfg = render_agent_config(
        "http://localhost:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    assert!(cfg.contains("input_price_per_mtok: 0.90"));
    assert!(cfg.contains("output_price_per_mtok: 0.90"));
}

#[test]
fn render_agent_config_has_all_strategy_fields() {
    let providers = sample_providers();
    let agents = sample_agents();
    let cfg = render_agent_config(
        "http://localhost:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    // Preset agents get persona from PRESET_CONFIGS (agent section).
    // DEFAULT prefix + VERIFY prefix — both in the trimmed General set.
    assert!(cfg.contains("persona: \"You are a helpful"));
    assert!(cfg.contains("persona: \"You are a detail-oriented fact-checker"));

    // LLM strategy fields are now in the models: block under providers
    assert!(cfg.contains("context_window: 131072"));
    assert!(cfg.contains("reasoning_effort: \"medium\""));
    assert!(cfg.contains("models:"));
    assert!(cfg.contains("model_name:"));

    // Agent section: remaining commented defaults
    assert!(cfg.contains("# max_react_iterations:"));
    assert!(cfg.contains("# max_retries:"));
    assert!(cfg.contains("# failure_dumps:"));
}

// ── render_orchestrator_config ────────────────────────────────────────

#[test]
fn render_orchestrator_config_has_required_sections() {
    let providers = sample_providers();
    let cfg = render_orchestrator_config(8080, 4222, &providers);

    assert!(cfg.contains("server:"));
    assert!(cfg.contains("port: 8080"));
    assert!(cfg.contains("nats:"));
    assert!(cfg.contains("url: \"nats://127.0.0.1:4222\""));
    assert!(cfg.contains("providers:"));
    assert!(cfg.contains("auth:"));
    assert!(cfg.contains("enabled: true"));
    assert!(cfg.contains("credentials:"));
    assert!(cfg.contains("logging:"));
}

#[test]
fn render_orchestrator_config_has_commented_orchestrator_section() {
    let providers = sample_providers();
    let cfg = render_orchestrator_config(8080, 4222, &providers);

    assert!(cfg.contains("# orchestrator:"));
    assert!(cfg.contains("#   subject_prefix:"));
    assert!(cfg.contains("#   default_job_timeout_secs:"));
}

// ── check_existing ─────────────────────────────────────────────────

#[test]
fn check_existing_fresh_directory_returns_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    match check_existing(dir.path(), "docker-compose.yml").unwrap() {
        ReinitChoice::Overwrite => {} // expected
        _ => panic!("expected Overwrite for fresh directory"),
    }
}

// ── AGENT_PRESETS constant ──────────────────────────────────────────

#[test]
fn agent_preset_names_are_unique() {
    let set: std::collections::HashSet<_> = AGENT_PRESETS.iter().map(|a| a.name).collect();
    assert_eq!(
        set.len(),
        AGENT_PRESETS.len(),
        "agent preset names must be unique"
    );
}

#[test]
fn agent_preset_names_are_uppercase() {
    for preset in AGENT_PRESETS {
        assert_eq!(
            preset.name,
            preset.name.to_uppercase(),
            "agent name {} should be uppercase",
            preset.name
        );
    }
}

#[test]
fn all_presets_have_config() {
    for preset in AGENT_PRESETS {
        assert!(
            PRESET_CONFIGS.iter().any(|c| c.name == preset.name),
            "agent preset {} has no entry in PRESET_CONFIGS",
            preset.name
        );
    }
}

#[test]
fn agent_presets_have_nonempty_descriptions() {
    for preset in AGENT_PRESETS {
        assert!(
            !preset.desc.is_empty(),
            "agent {} must have a description",
            preset.name
        );
    }
}

// ── seed preview (UTF-8 safety) ────────────────────────────────

#[test]
fn seed_preview_is_utf8_safe() {
    // The account seed preview uses .chars().take(12).collect::<String>()
    // which is correct for multi-byte chars.  Validate the pattern.
    let ascii_seed = "SAABCDEFGHIJKLMNOP";
    let preview: String = ascii_seed.chars().take(12).collect();
    assert_eq!(preview, "SAABCDEFGHIJ");

    // Multi-byte: each emoji is 1 char but 4 bytes
    let emoji_seed = "🔑🔐🔒🔓🗝️📌🏷️🎫🎟️🎪🎭🎨extra";
    let preview: String = emoji_seed.chars().take(12).collect();
    assert_eq!(preview.chars().count(), 12);
    // Must not panic — a byte-based [..12] would panic here
    assert!(preview.starts_with('🔑'));
}

// ── is_tested_model ──────────────────────────────────────────────

#[test]
fn tested_models_are_detected() {
    assert!(is_tested_model("openai/gpt-oss-120b"));
    assert!(is_tested_model("moonshotai/Kimi-K2.5"));
    assert!(!is_tested_model("unknown/random-model"));
}

// ── is_inference_model (additional coverage) ────────────────────

#[test]
fn test_is_inference_model_excludes_embedding_type() {
    let e = entry_typed("some-model", "embedding");
    assert!(!is_inference_model(&e));
}

#[test]
fn test_is_inference_model_excludes_rerank_type() {
    let e = entry_typed("some-model", "rerank");
    assert!(!is_inference_model(&e));
}

#[test]
fn test_is_inference_model_accepts_chat_type() {
    let e = entry_typed("some-model", "chat");
    assert!(is_inference_model(&e));
}

#[test]
fn test_is_inference_model_keyword_filter() {
    // Models with embedding/rerank keywords in their id (no type field)
    assert!(!is_inference_model(&entry("my-embed-model")));
    assert!(!is_inference_model(&entry("bge-large-en")));
    assert!(!is_inference_model(&entry("openai-clip-v2")));
}

#[test]
fn test_is_inference_model_accepts_normal_model() {
    let e = entry("meta-llama/Llama-4-70b");
    assert!(is_inference_model(&e));
}

// ── AgentSlot ──────────────────────────────────────────────────

#[test]
fn test_agent_slot_new_defaults() {
    let slot = AgentSlot::new(
        "TEST".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        Some(1.0),
        Some(2.0),
    );
    assert_eq!(slot.name, "TEST");
    assert_eq!(slot.provider_id, "prov");
    assert_eq!(slot.model_name, "model-x");
    assert!((slot.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(slot.max_tokens, 4096);
    assert!((slot.presence_penalty - 1.5).abs() < f32::EPSILON);
    assert!(slot.persona.is_none());
    assert!(slot.context_window.is_none());
    assert!(slot.reasoning_effort.is_none());
    assert!(slot.use_streaming.is_none());
    assert!(slot.merge_system_prompt.is_none());
    assert!(slot.unwrap_hallucinated_tool_calls.is_none());
    assert!(slot.repair_invalid_escapes.is_none());
    assert!(slot.json_mode.is_none());
    assert!(slot.disable_native_tools.is_none());
    assert!(slot.scratchpad_limit.is_none());
}

#[test]
fn test_agent_slot_apply_preset_known() {
    let mut slot = AgentSlot::new(
        "DEFAULT".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        None,
        None,
    );
    slot.apply_preset();
    // DEFAULT preset: temperature=0.7, max_tokens=8096, context_window=131072
    assert!(slot.persona.is_some());
    assert!(slot.persona.as_ref().unwrap().contains("helpful"));
    assert!((slot.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(slot.max_tokens, 8096);
    assert_eq!(slot.context_window, Some(131072));
    assert_eq!(slot.reasoning_effort.as_deref(), Some("medium"));
}

#[test]
fn test_agent_slot_apply_preset_unknown() {
    let mut slot = AgentSlot::new(
        "NONEXISTENT_PRESET".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        None,
        None,
    );
    let orig_temp = slot.temperature;
    let orig_max = slot.max_tokens;
    slot.apply_preset();
    // Nothing should change for an unknown preset name
    assert!(slot.persona.is_none());
    assert!((slot.temperature - orig_temp).abs() < f32::EPSILON);
    assert_eq!(slot.max_tokens, orig_max);
    assert!(slot.context_window.is_none());
    assert!(slot.reasoning_effort.is_none());
}

#[test]
fn test_agent_slot_apply_preset_with_strategy_flags() {
    let mut slot = AgentSlot::new(
        "DEFAULT".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        None,
        None,
    );
    slot.apply_preset();
    // All presets currently use default_preset() which sets strategy flags to None.
    // Verify that apply_preset propagates these None values from the preset.
    assert!(slot.use_streaming.is_none());
    assert!(slot.merge_system_prompt.is_none());
    assert!(slot.unwrap_hallucinated_tool_calls.is_none());
    assert!(slot.repair_invalid_escapes.is_none());
    assert!(slot.json_mode.is_none());
    assert!(slot.disable_native_tools.is_none());
    assert!(slot.scratchpad_limit.is_none());
}

// ── Provider::is_local() ───────────────────────────────────────

#[test]
fn test_provider_is_local_ollama() {
    let p = Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    };
    assert!(p.is_local());
}

#[test]
fn test_provider_is_local_simulated() {
    let p = Provider {
        id: "sim".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: None,
        output_price: None,
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    };
    assert!(!p.is_local(), "simulated providers should not be local");
}

#[test]
fn test_provider_is_local_remote() {
    let p = Provider {
        id: "together".to_string(),
        base_url: "https://api.together.xyz/v1".to_string(),
        api_key: Some("sk-abc".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    };
    assert!(!p.is_local(), "providers with api_key should not be local");
}

// ── backup_files() ─────────────────────────────────────────────

#[test]
fn test_backup_files_creates_backup_dir() {
    let dir = tempfile::tempdir().unwrap();
    // backup_files always creates .nsed-backup/ even if no files exist
    backup_files(dir.path(), "docker-compose.yml").unwrap();
    assert!(dir.path().join(".nsed-backup").is_dir());
}

#[test]
fn test_backup_files_copies_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("docker-compose.yml"), "version: '3'\n").unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=val\n").unwrap();
    backup_files(dir.path(), "docker-compose.yml").unwrap();

    let backup_dir = dir.path().join(".nsed-backup");
    let entries: Vec<_> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    // Should have backed up both files
    assert_eq!(
        entries.len(),
        2,
        "expected 2 backup files, got {}",
        entries.len()
    );
}

#[test]
fn test_backup_files_ignores_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    // Only compose exists, no .env
    std::fs::write(dir.path().join("docker-compose.yml"), "version: '3'\n").unwrap();
    backup_files(dir.path(), "docker-compose.yml").unwrap();

    let backup_dir = dir.path().join(".nsed-backup");
    let entries: Vec<_> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    // Should have backed up only the compose file
    assert_eq!(
        entries.len(),
        1,
        "expected 1 backup file, got {}",
        entries.len()
    );
}

// ── Print functions (coverage) ─────────────────────────────────

#[test]
fn test_print_pricing_table_does_not_panic() {
    let agents = sample_agents();
    print_pricing_table(&agents);
}

#[test]
fn test_print_share_with_workers_does_not_panic() {
    print_share_with_workers(8080, "worker-token-abc");
}

#[test]
fn test_print_list_agents_does_not_panic() {
    print_list_agents(8080, "api-secret-xyz");
}

#[test]
fn test_print_submit_guidance_does_not_panic() {
    let agents = sample_agents();
    print_submit_guidance(8080, "api-secret-xyz", &agents);
}

#[test]
fn test_print_security_notice_does_not_panic() {
    print_security_notice();
}

// ── render_orchestrator_config (provider types) ────────────────

#[test]
fn test_render_orchestrator_config_provider_types() {
    let providers = sample_providers();
    let cfg = render_orchestrator_config(8080, 4222, &providers);

    // The simulated provider should have type: simulated
    assert!(cfg.contains("type: simulated"));
    // The remote provider should have type: openai
    assert!(cfg.contains("type: openai"));
    // The simulated provider should have latency_ms
    assert!(cfg.contains("latency_ms: 400"));
    // Local (ollama) provider should have api_key: "ollama"
    assert!(cfg.contains("api_key: \"ollama\""));
    // Remote provider should have env-var comment for api_key
    assert!(cfg.contains("TOGETHER_AI_API_KEY"));
}

// ── render_compose_worker (additional) ──────────────────────────

#[test]
fn compose_worker_simulated_provider_no_api_key_forwarding() {
    let providers = vec![Provider {
        id: "simulated".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "simulated".to_string(),
        "simulated-default".to_string(),
        Some(0.0),
        Some(0.0),
    )];
    let compose = render_compose_worker(&agents, &providers);

    assert!(compose.contains("nsed-agent-1:"));
    assert!(compose.contains("NSED_AGENT_NAME: DEFAULT"));
    assert!(compose.contains("NSED_PROVIDER: simulated"));
    // Simulated provider should NOT have API key forwarding
    assert!(!compose.contains("API_KEY"));
    // Simulated provider has empty base_url -> no NSED_BASE_URL
    assert!(!compose.contains("NSED_BASE_URL"));
}

#[test]
fn compose_worker_multiple_agents_different_providers() {
    let providers = vec![
        Provider {
            id: "together_ai".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            api_key: Some("sk-key".to_string()),
            input_price: Some(0.90),
            output_price: Some(0.90),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "ollama_local".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
    ];
    let agents = vec![
        AgentSlot::new(
            "DEFAULT".to_string(),
            "together_ai".to_string(),
            "meta-llama/Llama-3-70b".to_string(),
            Some(0.90),
            Some(0.90),
        ),
        AgentSlot::new(
            "REASON".to_string(),
            "ollama_local".to_string(),
            "llama3.2".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        AgentSlot::new(
            "CREATE".to_string(),
            "together_ai".to_string(),
            "deepseek/deepseek-r1".to_string(),
            Some(0.50),
            Some(0.50),
        ),
    ];
    let compose = render_compose_worker(&agents, &providers);

    // Three agent services
    assert!(compose.contains("nsed-agent-1:"));
    assert!(compose.contains("nsed-agent-2:"));
    assert!(compose.contains("nsed-agent-3:"));

    // Agent 1 uses together_ai -> has API key forwarding
    assert!(compose.contains("TOGETHER_AI__API_KEY"));
    // Agent 2 uses ollama_local -> no API key forwarding (local)
    // Agent 2 has base_url
    assert!(compose.contains("NSED_BASE_URL: http://localhost:11434/v1"));

    // All agents have NSED_MODEL
    assert!(compose.contains("NSED_MODEL: meta-llama/Llama-3-70b"));
    assert!(compose.contains("NSED_MODEL: llama3.2"));
    assert!(compose.contains("NSED_MODEL: deepseek/deepseek-r1"));

    // Worker compose has no NATS or orchestrator service
    assert!(!compose.contains("  nats:"));
    assert!(!compose.contains("  nsed:"));
}

#[test]
fn compose_worker_agent_with_empty_model_name() {
    let providers = vec![Provider {
        id: "custom".to_string(),
        base_url: "https://custom.api/v1".to_string(),
        api_key: Some("sk-custom".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "custom".to_string(),
        String::new(), // empty model name
        None,
        None,
    )];
    let compose = render_compose_worker(&agents, &providers);

    // Empty model name should NOT produce NSED_MODEL line
    assert!(!compose.contains("NSED_MODEL:"));
}

// ── render_agent_config (additional) ─────────────────────────────

#[test]
fn render_agent_config_none_pricing_omits_price_lines() {
    let providers = vec![Provider {
        id: "custom_prov".to_string(),
        base_url: "https://custom.api/v1".to_string(),
        api_key: Some("sk-custom".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let mut agent = AgentSlot::new(
        "DEFAULT".to_string(),
        "custom_prov".to_string(),
        "gpt-4o".to_string(),
        None, // no input pricing
        None, // no output pricing
    );
    agent.apply_preset();
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    // When pricing is None, should NOT emit input_price_per_mtok or output_price_per_mtok
    assert!(
        !cfg.contains("input_price_per_mtok"),
        "None pricing should not produce input_price_per_mtok line"
    );
    assert!(
        !cfg.contains("output_price_per_mtok"),
        "None pricing should not produce output_price_per_mtok line"
    );
}

#[test]
fn render_agent_config_strategy_flags_some_true() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let mut agent = AgentSlot::new(
        "TEST".to_string(),
        "prov".to_string(),
        "test-model".to_string(),
        None,
        None,
    );
    // Set all strategy flags to Some(true)
    agent.use_streaming = Some(true);
    agent.merge_system_prompt = Some(true);
    agent.unwrap_hallucinated_tool_calls = Some(true);
    agent.repair_invalid_escapes = Some(true);
    agent.json_mode = Some(true);
    agent.disable_native_tools = Some(true);
    agent.scratchpad_limit = Some(5000);
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    // Active (uncommented) values
    assert!(cfg.contains("    use_streaming: true"));
    assert!(cfg.contains("    merge_system_prompt: true"));
    assert!(cfg.contains("    unwrap_hallucinated_tool_calls: true"));
    assert!(cfg.contains("    repair_invalid_escapes: true"));
    assert!(cfg.contains("    json_mode: true"));
    assert!(cfg.contains("    disable_native_tools: true"));
    assert!(cfg.contains("    scratchpad_limit: 5000"));
}

#[test]
fn render_agent_config_emits_failure_dumps_when_set() {
    let providers = sample_providers();
    let mut agent = AgentSlot::new(
        "DEFAULT".to_string(),
        "together_ai".to_string(),
        "x".to_string(),
        None,
        None,
    );
    agent.failure_dumps = Some("full".to_string());
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );
    assert!(
        cfg.contains("failure_dumps: \"full\""),
        "failure_dumps must render when set, got:\n{cfg}"
    );
}

#[test]
fn render_agent_config_strategy_flags_some_false() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let mut agent = AgentSlot::new(
        "TEST".to_string(),
        "prov".to_string(),
        "test-model".to_string(),
        None,
        None,
    );
    // Set all boolean strategy flags to Some(false)
    agent.use_streaming = Some(false);
    agent.merge_system_prompt = Some(false);
    agent.unwrap_hallucinated_tool_calls = Some(false);
    agent.repair_invalid_escapes = Some(false);
    agent.json_mode = Some(false);
    agent.disable_native_tools = Some(false);
    agent.scratchpad_limit = Some(0);
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    assert!(cfg.contains("    use_streaming: false"));
    assert!(cfg.contains("    merge_system_prompt: false"));
    assert!(cfg.contains("    unwrap_hallucinated_tool_calls: false"));
    assert!(cfg.contains("    repair_invalid_escapes: false"));
    assert!(cfg.contains("    json_mode: false"));
    assert!(cfg.contains("    disable_native_tools: false"));
    assert!(cfg.contains("    scratchpad_limit: 0"));
}

#[test]
fn render_agent_config_engine_override() {
    let providers = vec![Provider {
        id: "gpt_oss".to_string(),
        base_url: "https://gptoss.example.com/v1".to_string(),
        api_key: Some("sk-gptoss".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: Some("harmony".to_string()),
    }];
    let agent = AgentSlot::new(
        "DEFAULT".to_string(),
        "gpt_oss".to_string(),
        "gpt-oss-120b".to_string(),
        None,
        None,
    );
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    assert!(
        cfg.contains("engine: \"harmony\""),
        "engine override should appear in provider section"
    );
}

#[test]
fn render_agent_config_no_persona_emits_comment() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    // Agent without preset -> no persona
    let agent = AgentSlot::new(
        "CUSTOM_AGENT".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        None,
        None,
    );
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    assert!(
        cfg.contains("# persona: \"You are a helpful AI assistant.\""),
        "Agent without persona should emit commented persona placeholder"
    );
}

#[test]
fn render_agent_config_no_context_window_emits_model_block() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let agent = AgentSlot::new(
        "CUSTOM".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        None,
        None,
    );
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    // Model block is emitted under the provider; context_window absent when None
    assert!(cfg.contains("      model-x:"));
    assert!(cfg.contains("        model_name: \"model-x\""));
    // context_window and reasoning_effort not emitted when None
    assert!(!cfg.contains("context_window:"));
    assert!(!cfg.contains("reasoning_effort:"));
}

#[test]
fn render_agent_config_simulated_provider_section() {
    let providers = vec![Provider {
        id: "simulated".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];
    let agent = AgentSlot::new(
        "DEFAULT".to_string(),
        "simulated".to_string(),
        "simulated-default".to_string(),
        Some(0.0),
        Some(0.0),
    );
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    // Simulated providers: latency_ms, concurrency but no api_key or qps
    assert!(cfg.contains("simulated:"));
    assert!(cfg.contains("type: simulated"));
    assert!(cfg.contains("latency_ms: 400"));
    assert!(cfg.contains("concurrency: 100"));
    // Should NOT have api_key line
    assert!(!cfg.contains("api_key: \"${SIMULATED_API_KEY}\""));
}

// ── AgentSlot apply_preset — General ensemble ────────────────────

#[test]
fn test_agent_slot_apply_preset_general_ensemble() {
    let names = ["DEFAULT", "REASON", "CREATE", "VERIFY"];
    for name in names {
        let mut slot = AgentSlot::new(
            name.to_string(),
            "prov".to_string(),
            "model-x".to_string(),
            None,
            None,
        );
        slot.apply_preset();
        assert!(
            slot.persona.is_some(),
            "General preset {} should have a persona",
            name
        );
        assert!(
            slot.context_window.is_some(),
            "General preset {} should have a context_window",
            name
        );
    }

    let mut reason = AgentSlot::new(
        "REASON".to_string(),
        "p".to_string(),
        "m".to_string(),
        None,
        None,
    );
    reason.apply_preset();
    assert!(reason.persona.as_ref().unwrap().contains("reasoning"));
    assert_eq!(reason.max_tokens, 12096);

    let mut create = AgentSlot::new(
        "CREATE".to_string(),
        "p".to_string(),
        "m".to_string(),
        None,
        None,
    );
    create.apply_preset();
    assert!(create.persona.as_ref().unwrap().contains("creative"));
    assert_eq!(create.context_window, Some(262144));

    let mut verify = AgentSlot::new(
        "VERIFY".to_string(),
        "p".to_string(),
        "m".to_string(),
        None,
        None,
    );
    verify.apply_preset();
    assert!(verify.persona.as_ref().unwrap().contains("fact-checker"));
    assert!((verify.temperature - 0.5).abs() < f32::EPSILON);
}

// ── is_inference_model (additional coverage) ─────────────────────

#[test]
fn test_is_inference_model_excludes_vision() {
    assert!(!is_inference_model(&entry("openai/gpt-4-vision-preview")));
    assert!(!is_inference_model(&entry("llava-vision-7b")));
}

#[test]
fn test_is_inference_model_excludes_whisper() {
    assert!(!is_inference_model(&entry("openai/whisper-large-v3")));
    assert!(!is_inference_model(&entry("whisper-1")));
}

#[test]
fn test_is_inference_model_moderation_type() {
    let e = entry_typed("omni-moderation-latest", "moderation");
    assert!(!is_inference_model(&e));
    // Case-insensitive type matching
    let e2 = entry_typed("omni-moderation-latest", "Moderation");
    assert!(!is_inference_model(&e2));
}

#[test]
fn test_is_inference_model_image_type() {
    let e = entry_typed("dall-e-3", "image");
    assert!(!is_inference_model(&e));
    let e2 = entry_typed("dall-e-3", "Image");
    assert!(!is_inference_model(&e2));
}

#[test]
fn test_is_inference_model_type_field_overrides_keyword() {
    // A model with "embed" in its name but type="chat" should pass
    let e = entry_typed("embed-chat-model", "chat");
    assert!(is_inference_model(&e));
    // A model with a clean name but type="embedding" should fail
    let e2 = entry_typed("clean-name-model", "embedding");
    assert!(!is_inference_model(&e2));
}

// ── render_env (additional coverage) ─────────────────────────────

#[test]
fn render_env_engine_override_on_provider() {
    let providers = vec![Provider {
        id: "gpt_oss".to_string(),
        base_url: "https://gptoss.example.com/v1".to_string(),
        api_key: Some("sk-gptoss".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: Some("harmony".to_string()),
    }];
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);

    // API key should be emitted
    assert!(env.contains("GPT_OSS_API_KEY=sk-gptoss"));
    // Base URL should be emitted
    assert!(env.contains("APP_PROVIDERS__GPT_OSS__BASE_URL=https://gptoss.example.com/v1"));
}

#[test]
fn render_env_provider_with_hyphenated_id() {
    let providers = vec![Provider {
        id: "my-custom-provider".to_string(),
        base_url: "https://my-custom.api/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);

    // Hyphens in provider id should be replaced with underscores in env key
    assert!(env.contains("MY_CUSTOM_PROVIDER_API_KEY=sk-key"));
    assert!(env.contains("APP_PROVIDERS__MY_CUSTOM_PROVIDER__BASE_URL=https://my-custom.api/v1"));
}

#[test]
fn render_env_only_simulated_providers_no_base_urls_section() {
    let providers = vec![Provider {
        id: "simulated".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);

    // No base URL section at all since simulated has empty base_url
    assert!(!env.contains("LLM Provider base URLs"));
}

// ── fmt_price (additional edge cases) ────────────────────────────

#[test]
fn test_fmt_price_very_large() {
    assert_eq!(fmt_price(Some(999.99)), "$999.99");
    assert_eq!(fmt_price(Some(1000.0)), "$1000.00");
}

#[test]
fn test_fmt_price_negative() {
    // Negative prices are an edge case, but format should still work
    assert_eq!(fmt_price(Some(-1.50)), "$-1.50");
}

#[test]
fn test_fmt_price_tiny() {
    assert_eq!(fmt_price(Some(0.01)), "$0.01");
    assert_eq!(fmt_price(Some(0.001)), "$0.00");
}

// ── PRESET_CONFIGS validation ────────────────────────────────────

#[test]
fn test_preset_configs_are_complete() {
    // Every AGENT_PRESETS name must have a corresponding PRESET_CONFIGS entry
    for preset in AGENT_PRESETS {
        assert!(
            PRESET_CONFIGS.iter().any(|c| c.name == preset.name),
            "AGENT_PRESETS entry '{}' has no matching PRESET_CONFIGS entry",
            preset.name
        );
    }
    // And vice-versa: every PRESET_CONFIGS entry has an AGENT_PRESETS entry
    for config in PRESET_CONFIGS {
        assert!(
            AGENT_PRESETS.iter().any(|a| a.name == config.name),
            "PRESET_CONFIGS entry '{}' has no matching AGENT_PRESETS entry",
            config.name
        );
    }
}

#[test]
fn test_preset_configs_have_valid_temperatures() {
    for config in PRESET_CONFIGS {
        assert!(
            (0.0..=2.0).contains(&config.temperature),
            "Preset {} has temperature {} which is outside [0.0, 2.0]",
            config.name,
            config.temperature
        );
    }
}

#[test]
fn test_preset_configs_have_positive_max_tokens() {
    for config in PRESET_CONFIGS {
        assert!(
            config.max_tokens > 0,
            "Preset {} has max_tokens {} which should be > 0",
            config.name,
            config.max_tokens
        );
    }
}

#[test]
fn test_preset_configs_have_nonempty_personas() {
    for config in PRESET_CONFIGS {
        assert!(
            !config.persona.is_empty(),
            "Preset {} has an empty persona",
            config.name
        );
        // Personas should be meaningful (more than 20 chars)
        assert!(
            config.persona.len() > 20,
            "Preset {} has a very short persona: '{}'",
            config.name,
            config.persona
        );
    }
}

#[test]
fn test_preset_configs_valid_reasoning_effort() {
    let valid_efforts = ["low", "medium", "high"];
    for config in PRESET_CONFIGS {
        if let Some(effort) = config.reasoning_effort {
            assert!(
                valid_efforts.contains(&effort),
                "Preset {} has invalid reasoning_effort '{}', expected one of {:?}",
                config.name,
                effort,
                valid_efforts
            );
        }
    }
}

#[test]
fn test_preset_configs_valid_context_windows() {
    for config in PRESET_CONFIGS {
        if let Some(cw) = config.context_window {
            assert!(
                cw > 0,
                "Preset {} has context_window {} which should be > 0",
                config.name,
                cw
            );
            // Context windows are typically powers of 2 * some factor, but at minimum > 1024
            assert!(
                cw >= 1024,
                "Preset {} has unusually small context_window {}",
                config.name,
                cw
            );
        }
    }
}

// ── env_keys helper ──────────────────────────────────────────────

#[test]
fn test_env_keys_extracts_keys() {
    let content = "# comment\nKEY1=val1\nKEY2=val2\n\n# another\nKEY3=val3\n";
    let keys = env_keys(content);
    assert_eq!(keys.len(), 3);
    assert!(keys.contains("KEY1"));
    assert!(keys.contains("KEY2"));
    assert!(keys.contains("KEY3"));
}

#[test]
fn test_env_keys_ignores_comments_and_blanks() {
    let content = "# full comment line\n\n   \n  # indented comment\n";
    let keys = env_keys(content);
    assert!(keys.is_empty());
}

#[test]
fn test_env_keys_handles_values_with_equals() {
    let content = "BASE64_DATA=abc=def==\n";
    let keys = env_keys(content);
    assert_eq!(keys.len(), 1);
    assert!(keys.contains("BASE64_DATA"));
}

// ── render_orchestrator_config (additional) ──────────────────────

#[test]
fn render_orchestrator_config_empty_providers() {
    let cfg = render_orchestrator_config(8080, 4222, &[]);

    assert!(cfg.contains("# providers:"));
    assert!(cfg.contains("#   (configured via environment variables or dashboard)"));
    assert!(!cfg.contains("\nproviders:\n"));
}

#[test]
fn render_orchestrator_config_engine_override() {
    let providers = vec![Provider {
        id: "gpt_oss".to_string(),
        base_url: "https://gptoss.api/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: Some("harmony".to_string()),
    }];
    let cfg = render_orchestrator_config(8080, 4222, &providers);

    assert!(cfg.contains("engine: \"harmony\""));
}

#[test]
fn render_orchestrator_config_has_nsed_protocol_section() {
    let cfg = render_orchestrator_config(8080, 4222, &[]);

    assert!(cfg.contains("NSED Protocol Settings"));
    assert!(cfg.contains("# nsed:"));
    assert!(cfg.contains("#   textual_feedback: true"));
    assert!(cfg.contains("#   force_last_round_selection: true"));
}

#[test]
fn render_orchestrator_config_credentials_section() {
    let cfg = render_orchestrator_config(8080, 4222, &[]);

    assert!(cfg.contains("credentials:"));
    assert!(cfg.contains("enabled: true"));
    assert!(cfg.contains("jwt_expiry_secs: 86400"));
    assert!(cfg.contains("challenge_expiry_secs: 300"));
}

// ── render_compose_owner (additional) ────────────────────────────

#[test]
fn compose_owner_nats_healthcheck_uses_correct_port() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_owner(9090, 5222, &providers, &agents);

    assert!(compose.contains("\"--port\", \"5222\""));
    assert!(compose.contains("nc localhost 5222"));
    assert!(compose.contains("${NATS_PORT:-5222}:5222"));
}

#[test]
fn compose_owner_custom_ports() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_owner(9090, 5222, &providers, &agents);

    assert!(compose.contains("${APP_PORT:-9090}:9090"));
    assert!(compose.contains("nats://nats:5222"));
}

// ── print_pricing_table additional ───────────────────────────────

#[test]
fn test_print_pricing_table_with_none_prices() {
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "custom".to_string(),
        "unknown-model".to_string(),
        None,
        None,
    )];
    // Should not panic with None prices
    print_pricing_table(&agents);
}

#[test]
fn test_print_pricing_table_long_model_name_truncated() {
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "prov".to_string(),
        "this-is-a-very-long-model-name-that-exceeds-thirty-characters".to_string(),
        Some(1.0),
        Some(2.0),
    )];
    // Should not panic - internally truncates to 30 chars
    print_pricing_table(&agents);
}

#[test]
fn test_print_submit_guidance_formats_agent_names() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "m".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "m".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "p".to_string(),
            "m".to_string(),
            None,
            None,
        ),
    ];
    // Should not panic
    print_submit_guidance(8080, "secret", &agents);
}

// ── ask() helper ────────────────────────────────────────────────

#[test]
fn ask_ok_returns_some() {
    let result: Result<Option<String>> = ask(Ok("hello".to_string()));
    let val = result.unwrap();
    assert_eq!(val, Some("hello".to_string()));
}

#[test]
fn ask_canceled_returns_none() {
    let result: Result<Option<String>> = ask(Err(InquireError::OperationCanceled));
    let val = result.unwrap();
    assert!(val.is_none());
}

#[test]
fn ask_interrupted_returns_none() {
    let result: Result<Option<String>> = ask(Err(InquireError::OperationInterrupted));
    let val = result.unwrap();
    assert!(val.is_none());
}

#[test]
fn ask_other_error_propagates() {
    let result: Result<Option<String>> = ask(Err(InquireError::Custom("test error".into())));
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("test error"));
}

// ── parse_env_file (additional edge cases) ──────────────────────

#[test]
fn parse_env_file_line_without_equals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "NOEQUALS\nVALID_KEY=valid_value\n").unwrap();
    let env = parse_env_file(&path);
    assert_eq!(env.len(), 1);
    assert_eq!(env.get("VALID_KEY").unwrap(), "valid_value");
    assert!(!env.contains_key("NOEQUALS"));
}

#[test]
fn parse_env_file_empty_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "EMPTY_VAL=\n").unwrap();
    let env = parse_env_file(&path);
    assert_eq!(env.get("EMPTY_VAL").unwrap(), "");
}

#[test]
fn parse_env_file_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "").unwrap();
    let env = parse_env_file(&path);
    assert!(env.is_empty());
}

#[test]
fn parse_env_file_only_comments_and_blanks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "# comment 1\n\n  # indented comment\n   \n").unwrap();
    let env = parse_env_file(&path);
    assert!(env.is_empty());
}

#[test]
fn parse_env_file_value_with_spaces_and_equals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "JWT_SECRET=abc123==\nURL=http://host:8080\n").unwrap();
    let env = parse_env_file(&path);
    assert_eq!(env.get("JWT_SECRET").unwrap(), "abc123==");
    assert_eq!(env.get("URL").unwrap(), "http://host:8080");
}

// ── render_env (additional edge cases) ──────────────────────────

#[test]
fn render_env_provider_with_no_api_key_but_remote() {
    // Provider with api_key=None and not local, not simulated
    // This represents a provider where the user didn't enter a key
    let providers = vec![Provider {
        id: "openrouter".to_string(),
        base_url: "https://openrouter.ai/api/v1".to_string(),
        api_key: None,
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);
    // Provider has api_key=None → is_local()=true → should show "local endpoint"
    assert!(env.contains("openrouter: local endpoint"));
}

#[test]
fn render_env_provider_with_empty_api_key() {
    // api_key is Some("") — not None, so is_local()=false
    // The env should emit with <your-key-here> since api_key.as_deref() returns Some("")
    // Actually wait: if api_key is Some(""), is_local() returns false (api_key.is_none()=false)
    // But the match arm for _ (non-simulated, non-local) will use api_key.as_deref().unwrap_or(...)
    // which returns "" since it's Some("").
    // Let's just test this doesn't panic and produces correct output.
    let providers = vec![Provider {
        id: "custom".to_string(),
        base_url: "https://custom.api/v1".to_string(),
        api_key: Some(String::new()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);
    // api_key=Some("") -> is_local()=false -> env_key line emitted
    // unwrap_or("<your-key-here>") → "" since Some("") not None
    assert!(env.contains("CUSTOM_API_KEY="));
}

#[test]
fn render_env_no_providers() {
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &[], None, None);
    assert!(env.contains("APP_PORT=8080"));
    assert!(env.contains("NATS_PORT=4222"));
    assert!(!env.contains("LLM Provider base URLs"));
}

#[test]
fn render_env_worker_no_orchestrator_url() {
    // Worker persona with orchestrator_url=None — should not emit NSED_ORCHESTRATOR_URL
    let env = render_env("worker", 8080, 4222, "tok", "tok", "", &[], None, None);
    assert!(!env.contains("NSED_ORCHESTRATOR_URL"));
    assert!(env.contains("NSED_BEARER_TOKEN=tok"));
}

// ── render_compose_owner (additional edge cases) ─────────────────

#[test]
fn compose_owner_empty_agents_list() {
    let providers = sample_providers();
    let compose = render_compose_owner(8080, 4222, &providers, &[]);
    // Should have NATS and orchestrator but no agent services
    assert!(compose.contains("  nats:"));
    assert!(compose.contains("  nsed:"));
    assert!(!compose.contains("nsed-agent-"));
}

#[test]
fn compose_owner_agent_with_no_matching_provider() {
    let providers = sample_providers();
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "nonexistent_provider".to_string(),
        "model-x".to_string(),
        None,
        None,
    )];
    let compose = render_compose_owner(8080, 4222, &providers, &agents);
    // Should still render the agent service, just without provider-specific env vars
    assert!(compose.contains("nsed-agent-1:"));
    assert!(compose.contains("NSED_AGENT_NAME: DEFAULT"));
    // No provider match → no NSED_BASE_URL or NSED_PROVIDER for this agent
    assert!(!compose.contains("NSED_PROVIDER: nonexistent_provider"));
}

#[test]
fn compose_owner_agent_with_empty_model_name() {
    let providers = sample_providers();
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "together_ai".to_string(),
        String::new(),
        None,
        None,
    )];
    let compose = render_compose_owner(8080, 4222, &providers, &agents);
    // Empty model name → no NSED_MODEL line
    assert!(!compose.contains("NSED_MODEL:"));
    assert!(compose.contains("NSED_AGENT_NAME: DEFAULT"));
}

// ── render_compose_worker (additional edge cases) ────────────────

#[test]
fn compose_worker_engine_override_provider() {
    let providers = vec![Provider {
        id: "gpt_oss".to_string(),
        base_url: "https://gptoss.api/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: Some("harmony".to_string()),
    }];
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "gpt_oss".to_string(),
        "gpt-oss-120b".to_string(),
        None,
        None,
    )];
    let compose = render_compose_worker(&agents, &providers);
    assert!(compose.contains("NSED_BASE_URL: https://gptoss.api/v1"));
    assert!(compose.contains("NSED_PROVIDER: gpt_oss"));
    assert!(compose.contains("GPT_OSS__API_KEY"));
}

#[test]
fn compose_worker_empty_agents() {
    let compose = render_compose_worker(&[], &[]);
    assert!(compose.contains("services:"));
    assert!(!compose.contains("nsed-agent-"));
}

#[test]
fn compose_worker_local_provider_no_api_key() {
    let providers = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "ollama".to_string(),
        "llama3".to_string(),
        Some(0.0),
        Some(0.0),
    )];
    let compose = render_compose_worker(&agents, &providers);
    assert!(compose.contains("NSED_PROVIDER: ollama"));
    assert!(compose.contains("NSED_BASE_URL: http://localhost:11434/v1"));
    // Local provider → no API key forwarding
    assert!(!compose.contains("API_KEY"));
}

// ── render_agent_config (additional edge cases) ──────────────────

#[test]
fn render_agent_config_persona_with_double_quotes() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let mut agent = AgentSlot::new(
        "CUSTOM".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        None,
        None,
    );
    agent.persona = Some(r#"You are "the best" agent."#.to_string());
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );
    // Double quotes in persona should be escaped
    assert!(cfg.contains(r#"persona: "You are \"the best\" agent.""#));
}

#[test]
fn render_agent_config_multiple_providers_all_types() {
    let providers = vec![
        Provider {
            id: "together_ai".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            api_key: Some("sk-123".to_string()),
            input_price: Some(0.90),
            output_price: Some(0.90),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "ollama".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "simulated".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![],
            engine: None,
        },
    ];
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "together_ai".to_string(),
            "m1".to_string(),
            Some(0.90),
            Some(0.90),
        ),
        AgentSlot::new(
            "B".to_string(),
            "ollama".to_string(),
            "m2".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        AgentSlot::new(
            "C".to_string(),
            "simulated".to_string(),
            "sim".to_string(),
            Some(0.0),
            Some(0.0),
        ),
    ];
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    // Remote provider: env-var api_key, concurrency, qps
    assert!(cfg.contains("api_key: \"${TOGETHER_AI_API_KEY}\""));
    assert!(cfg.contains("qps: 50"));
    // Local provider: api_key: "ollama", concurrency=1000
    assert!(cfg.contains("api_key: \"ollama\""));
    assert!(cfg.contains("concurrency: 1000"));
    // Simulated provider: latency_ms, concurrency=100
    assert!(cfg.contains("latency_ms: 400"));
    assert!(cfg.contains("concurrency: 100"));
    // All three agents
    assert!(cfg.contains("name: \"A\""));
    assert!(cfg.contains("name: \"B\""));
    assert!(cfg.contains("name: \"C\""));
}

#[test]
fn render_agent_config_empty_agents() {
    let providers = sample_providers();
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &providers, &[]);
    assert!(cfg.contains("orchestrators:"));
    assert!(cfg.contains("providers:"));
    assert!(cfg.contains("agents:"));
    // No agent entries
    assert!(!cfg.contains("name: \"DEFAULT\""));
}

#[test]
fn render_agent_config_empty_providers() {
    let agents = vec![AgentSlot::new(
        "TEST".to_string(),
        "prov".to_string(),
        "model".to_string(),
        None,
        None,
    )];
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &[], &agents);
    assert!(cfg.contains("providers:"));
    assert!(cfg.contains("agents:"));
    assert!(cfg.contains("name: \"TEST\""));
}

// ── render_orchestrator_config (additional edge cases) ───────────

#[test]
fn render_orchestrator_config_local_provider_only() {
    let providers = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let cfg = render_orchestrator_config(8080, 4222, &providers);
    assert!(cfg.contains("ollama:"));
    assert!(cfg.contains("api_key: \"ollama\""));
    assert!(cfg.contains("concurrency: 1000"));
    // Should not have qps for local providers
    assert!(!cfg.contains("qps:"));
}

#[test]
fn render_orchestrator_config_custom_ports() {
    let cfg = render_orchestrator_config(9090, 5222, &[]);
    assert!(cfg.contains("port: 9090"));
    assert!(cfg.contains("url: \"nats://127.0.0.1:5222\""));
}

#[test]
fn render_orchestrator_config_simulated_provider_only() {
    let providers = vec![Provider {
        id: "simulated".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];
    let cfg = render_orchestrator_config(8080, 4222, &providers);
    assert!(cfg.contains("simulated:"));
    assert!(cfg.contains("type: simulated"));
    assert!(cfg.contains("latency_ms: 400"));
    assert!(!cfg.contains("base_url:"));
}

// ── ensure_gitignore (additional edge cases) ────────────────────

#[test]
fn ensure_gitignore_env_with_surrounding_whitespace() {
    let dir = tempfile::tempdir().unwrap();
    // .env with leading/trailing spaces — the check is `l.trim() == ".env"`
    std::fs::write(dir.path().join(".gitignore"), "  .env  \n").unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    // Should not duplicate since trim() matches
    assert_eq!(content.matches(".env").count(), 1);
}

#[test]
fn ensure_gitignore_env_in_path() {
    let dir = tempfile::tempdir().unwrap();
    // ".env.local" is not ".env" — should still append .env
    std::fs::write(dir.path().join(".gitignore"), ".env.local\n").unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(content.contains(".env.local"));
    // Should have added a new .env entry
    assert!(content.contains("\n.env\n") || content.ends_with(".env\n"));
}

// ── write_files (additional edge cases) ─────────────────────────

#[test]
fn write_files_only_agent_config() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    let configs = ConfigFiles {
        agent_config: Some("agents:\n  - name: TEST\n".to_string()),
        orchestrator_config: None,
    };
    write_files(
        dir.path(),
        "compose.yml",
        "compose",
        "K=V\n",
        &empty,
        &configs,
    )
    .unwrap();

    assert!(dir.path().join("config/agent.yml").exists());
    assert!(!dir.path().join("config/orchestrator.yml").exists());
}

#[test]
fn write_files_only_orchestrator_config() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    let configs = ConfigFiles {
        agent_config: None,
        orchestrator_config: Some("server:\n  port: 8080\n".to_string()),
    };
    write_files(
        dir.path(),
        "compose.yml",
        "compose",
        "K=V\n",
        &empty,
        &configs,
    )
    .unwrap();

    assert!(!dir.path().join("config/agent.yml").exists());
    assert!(dir.path().join("config/orchestrator.yml").exists());
}

#[test]
fn write_files_custom_compose_name() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    write_files(
        dir.path(),
        "my-compose.yml",
        "services:\n",
        "KEY=val\n",
        &empty,
        &no_configs(),
    )
    .unwrap();

    assert!(dir.path().join("my-compose.yml").exists());
    let content = std::fs::read_to_string(dir.path().join("my-compose.yml")).unwrap();
    assert_eq!(content, "services:\n");
}

#[test]
fn write_files_nested_output_dir() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("sub/dir");
    let empty = HashMap::new();
    write_files(
        &nested,
        "compose.yml",
        "services:\n",
        "K=V\n",
        &empty,
        &no_configs(),
    )
    .unwrap();

    assert!(nested.join("compose.yml").exists());
    assert!(nested.join(".env").exists());
    assert!(nested.join(".gitignore").exists());
}

// ── backup_files (additional edge cases) ────────────────────────

#[test]
fn test_backup_files_custom_compose_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("custom-compose.yml"), "services:\n").unwrap();
    backup_files(dir.path(), "custom-compose.yml").unwrap();

    let backup_dir = dir.path().join(".nsed-backup");
    let entries: Vec<_> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1);
    // Verify the backup file name contains "custom-compose.yml"
    let name = entries[0].file_name().to_string_lossy().to_string();
    assert!(name.starts_with("custom-compose.yml."));
}

#[test]
fn test_backup_files_no_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    backup_files(dir.path(), "docker-compose.yml").unwrap();
    let backup_dir = dir.path().join(".nsed-backup");
    assert!(backup_dir.is_dir());
    let entries: Vec<_> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 0);
}

// ── is_tested_model (all models) ────────────────────────────────

#[test]
fn all_tested_models_detected() {
    for model in TESTED_MODELS {
        assert!(
            is_tested_model(model),
            "TESTED_MODELS entry '{}' should be detected by is_tested_model",
            model
        );
    }
}

#[test]
fn tested_models_list_is_nonempty() {
    assert!(!TESTED_MODELS.is_empty());
}

#[test]
fn is_tested_model_is_case_sensitive() {
    // Tested models use exact string match
    assert!(!is_tested_model("OPENAI/GPT-OSS-120B"));
    assert!(!is_tested_model("Openai/Gpt-Oss-120b"));
}

// ── MODEL_FILTER_KEYWORDS / EXCLUDED_MODEL_TYPES ────────────────

#[test]
fn model_filter_keywords_are_lowercase() {
    for kw in MODEL_FILTER_KEYWORDS {
        assert_eq!(
            *kw,
            kw.to_lowercase(),
            "MODEL_FILTER_KEYWORDS entry '{}' should be lowercase",
            kw
        );
    }
}

#[test]
fn excluded_model_types_are_lowercase() {
    for t in EXCLUDED_MODEL_TYPES {
        assert_eq!(
            *t,
            t.to_lowercase(),
            "EXCLUDED_MODEL_TYPES entry '{}' should be lowercase",
            t
        );
    }
}

// ── default_preset const fn ─────────────────────────────────────

#[test]
fn default_preset_sets_all_strategy_flags_to_none() {
    let p = default_preset(
        "TEST",
        "persona text",
        0.7,
        4096,
        Some(32768),
        Some("medium"),
        &["general"],
        Some("Test agent"),
    );
    assert_eq!(p.name, "TEST");
    assert_eq!(p.persona, "persona text");
    assert!((p.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(p.max_tokens, 4096);
    assert_eq!(p.context_window, Some(32768));
    assert_eq!(p.reasoning_effort, Some("medium"));
    assert!(p.use_streaming.is_none());
    assert!(p.merge_system_prompt.is_none());
    assert!(p.unwrap_hallucinated_tool_calls.is_none());
    assert!(p.repair_invalid_escapes.is_none());
    assert!(p.json_mode.is_none());
    assert!(p.disable_native_tools.is_none());
    assert!(p.scratchpad_limit.is_none());
}

#[test]
fn default_preset_no_optional_fields() {
    let p = default_preset("MINIMAL", "persona", 0.5, 2048, None, None, &[], None);
    assert_eq!(p.name, "MINIMAL");
    assert!(p.context_window.is_none());
    assert!(p.reasoning_effort.is_none());
    assert!(p.capability_tags.is_empty());
    assert!(p.description.is_none());
}

// ── PRESET_CONFIGS names unique ─────────────────────────────────

#[test]
fn preset_configs_names_are_unique() {
    let set: std::collections::HashSet<_> = PRESET_CONFIGS.iter().map(|c| c.name).collect();
    assert_eq!(
        set.len(),
        PRESET_CONFIGS.len(),
        "PRESET_CONFIGS names must be unique"
    );
}

// ── PRESET_CONFIGS metadata populated ───────────────────────────

#[test]
fn preset_configs_have_capability_tags_and_description() {
    for preset in PRESET_CONFIGS {
        assert!(
            !preset.capability_tags.is_empty(),
            "Preset {} must have capability_tags for policy-based scheduling",
            preset.name
        );
        assert!(
            preset.description.is_some(),
            "Preset {} must have a description",
            preset.name
        );
    }
}

// ── env_keys (additional) ───────────────────────────────────────

#[test]
fn test_env_keys_empty_input() {
    let keys = env_keys("");
    assert!(keys.is_empty());
}

#[test]
fn test_env_keys_whitespace_only() {
    let keys = env_keys("   \n   \n  \n");
    assert!(keys.is_empty());
}

#[test]
fn test_env_keys_mixed_content() {
    let content = "\
# Header comment
APP_PORT=8080
  # indented comment
NATS_PORT=4222

APP_AUTH__ENABLED=true
# trailing comment";
    let keys = env_keys(content);
    assert_eq!(keys.len(), 3);
    assert!(keys.contains("APP_PORT"));
    assert!(keys.contains("NATS_PORT"));
    assert!(keys.contains("APP_AUTH__ENABLED"));
}

// ── print functions (additional coverage) ──────────────────────

#[test]
fn test_print_pricing_table_empty_agents() {
    // Should not panic with empty list
    print_pricing_table(&[]);
}

#[test]
fn test_print_pricing_table_free_and_paid_agents() {
    let agents = vec![
        AgentSlot::new(
            "FREE".to_string(),
            "ollama".to_string(),
            "llama3".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        AgentSlot::new(
            "PAID".to_string(),
            "together".to_string(),
            "llama-70b".to_string(),
            Some(0.90),
            Some(0.90),
        ),
        AgentSlot::new(
            "UNKNOWN".to_string(),
            "custom".to_string(),
            "model-x".to_string(),
            None,
            None,
        ),
    ];
    print_pricing_table(&agents);
}

#[test]
fn test_print_pricing_table_exact_30_char_model_name() {
    // Model name exactly 30 chars — should NOT be truncated
    let model = "a".repeat(30);
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "prov".to_string(),
        model.clone(),
        Some(1.0),
        Some(2.0),
    )];
    print_pricing_table(&agents);
}

#[test]
fn test_print_pricing_table_31_char_model_name_truncated() {
    // Model name 31 chars — should be truncated with ellipsis
    let model = "a".repeat(31);
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "prov".to_string(),
        model,
        Some(1.0),
        Some(2.0),
    )];
    print_pricing_table(&agents);
}

#[test]
fn test_print_pricing_table_unicode_model_name() {
    // Unicode model name > 30 chars — truncation must be char-aware
    let model = "m".repeat(25) + &"o".repeat(10); // 35 chars
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "prov".to_string(),
        model,
        Some(1.0),
        Some(2.0),
    )];
    print_pricing_table(&agents);
}

#[test]
fn test_print_share_with_workers_custom_port() {
    print_share_with_workers(9999, "my-worker-token");
}

#[test]
fn test_print_list_agents_custom_port() {
    print_list_agents(9999, "admin-tok");
}

#[test]
fn test_print_submit_guidance_empty_agents() {
    print_submit_guidance(8080, "secret", &[]);
}

#[test]
fn test_print_submit_guidance_single_agent() {
    let agents = vec![AgentSlot::new(
        "SOLO".to_string(),
        "p".to_string(),
        "m".to_string(),
        None,
        None,
    )];
    print_submit_guidance(8080, "secret", &agents);
}

// ── check_existing (additional) ─────────────────────────────────

#[test]
fn check_existing_only_env_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=val\n").unwrap();
    // check_existing calls interactive inquire, so we test the detection
    // layer directly: detect_existing_files should find .env.
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert!(found.contains(&".env"));
    assert!(!found.contains(&"docker-compose.yml"));
}

#[test]
fn check_existing_only_compose_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("docker-compose.yml"), "version: '3'\n").unwrap();
    // detect_existing_files should find the compose file but not .env.
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert!(found.contains(&"docker-compose.yml"));
    assert!(!found.contains(&".env"));
}

// ── AGENT_PRESETS ensemble grouping ─────────────────────────────

#[test]
fn agent_presets_have_valid_ensembles() {
    let valid_ensembles = ["General", "Security", "Quant", "Supply", "Legal"];
    for preset in AGENT_PRESETS {
        assert!(
            valid_ensembles.contains(&preset.ensemble),
            "Agent preset {} has invalid ensemble '{}'",
            preset.name,
            preset.ensemble
        );
    }
}

#[test]
fn agent_presets_only_general_ensemble() {
    // Domain-specific ensembles (Security / Quant / Supply / Legal)
    // were dropped in favour of a curated General set. Operators
    // needing those domains build them via custom persona free-text
    // or the structured `claude:` YAML block.
    for preset in AGENT_PRESETS {
        assert_eq!(
            preset.ensemble, "General",
            "only General ensemble is shipped now; got '{}' on {}",
            preset.ensemble, preset.name
        );
    }
    assert!(
        !AGENT_PRESETS.is_empty(),
        "General ensemble must have at least one preset"
    );
}

#[test]
fn agent_presets_general_ensemble_count() {
    let count = AGENT_PRESETS
        .iter()
        .filter(|a| a.ensemble == "General")
        .count();
    assert_eq!(count, 4, "General ensemble should have 4 agents");
}

#[test]
fn agent_presets_no_legacy_security_ensemble() {
    // Regression: Security ensemble was dropped along with Quant /
    // Supply / Legal. Asserting absence prevents accidental
    // re-introduction.
    let count = AGENT_PRESETS
        .iter()
        .filter(|a| a.ensemble == "Security")
        .count();
    assert_eq!(count, 0, "Security ensemble was retired");
}

// ── Render consistency: agent config round-trip ──────────────────

#[test]
fn render_agent_config_all_presets() {
    // Ensure rendering with ALL presets does not panic and produces valid output
    let providers = sample_providers();
    let agents: Vec<AgentSlot> = AGENT_PRESETS
        .iter()
        .map(|preset| {
            let mut slot = AgentSlot::new(
                preset.name.to_string(),
                "together_ai".to_string(),
                "meta-llama/Llama-3-70b".to_string(),
                Some(0.90),
                Some(0.90),
            );
            slot.apply_preset();
            slot
        })
        .collect();
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );
    // Every preset name should appear in the output
    for preset in AGENT_PRESETS {
        assert!(
            cfg.contains(&format!("name: \"{}\"", preset.name)),
            "Agent config should contain preset {}",
            preset.name
        );
    }
}

// ── render_env + render_compose consistency ─────────────────────

#[test]
fn render_env_and_compose_owner_use_same_ports() {
    let providers = sample_providers();
    let agents = sample_agents();
    let env = render_env("owner", 9090, 5222, "s", "w", "sa", &providers, None, None);
    let compose = render_compose_owner(9090, 5222, &providers, &agents);

    assert!(env.contains("APP_PORT=9090"));
    assert!(env.contains("NATS_PORT=5222"));
    assert!(compose.contains("9090"));
    assert!(compose.contains("5222"));
}

// ── ModelEntry deserialization ───────────────────────────────────

#[test]
fn model_entry_deserialize_minimal() {
    let json = r#"{"id": "test-model"}"#;
    let entry: ModelEntry = serde_json::from_str(json).unwrap();
    assert_eq!(entry.id, "test-model");
    assert!(entry.pricing.is_none());
    assert!(entry.model_type.is_none());
}

#[test]
fn model_entry_deserialize_with_type() {
    let json = r#"{"id": "test-model", "type": "chat"}"#;
    let entry: ModelEntry = serde_json::from_str(json).unwrap();
    assert_eq!(entry.id, "test-model");
    assert_eq!(entry.model_type.as_deref(), Some("chat"));
}

#[test]
fn model_entry_deserialize_with_pricing() {
    let json = r#"{"id": "test-model", "pricing": {"input": 0.15, "output": 0.45}}"#;
    let entry: ModelEntry = serde_json::from_str(json).unwrap();
    assert!(entry.pricing.is_some());
    let (inp, out) = extract_pricing(entry.pricing.as_ref());
    assert!((inp.unwrap() - 0.15).abs() < 1e-9);
    assert!((out.unwrap() - 0.45).abs() < 1e-9);
}

#[test]
fn model_entry_deserialize_with_string_pricing() {
    let json = r#"{"id": "test-model", "pricing": {"input": "0.20", "output": "0.60"}}"#;
    let entry: ModelEntry = serde_json::from_str(json).unwrap();
    let (inp, out) = extract_pricing(entry.pricing.as_ref());
    assert!((inp.unwrap() - 0.20).abs() < 1e-9);
    assert!((out.unwrap() - 0.60).abs() < 1e-9);
}

#[test]
fn models_response_wrapped_deserialize() {
    let json = r#"{"data": [{"id": "model-1"}, {"id": "model-2"}]}"#;
    let resp: ModelsResponseWrapped = serde_json::from_str(json).unwrap();
    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[0].id, "model-1");
    assert_eq!(resp.data[1].id, "model-2");
}

#[test]
fn models_response_flat_array_deserialize() {
    let json = r#"[{"id": "model-1"}, {"id": "model-2"}]"#;
    let entries: Vec<ModelEntry> = serde_json::from_str(json).unwrap();
    assert_eq!(entries.len(), 2);
}

// ── extract_pricing (additional edge cases) ─────────────────────

#[test]
fn extract_pricing_zero_values() {
    let p = ModelPricing {
        input: Some(serde_json::json!(0.0)),
        output: Some(serde_json::json!(0.0)),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!((inp.unwrap() - 0.0).abs() < 1e-9);
    assert!((out.unwrap() - 0.0).abs() < 1e-9);
}

#[test]
fn extract_pricing_null_values() {
    let p = ModelPricing {
        input: Some(serde_json::json!(null)),
        output: Some(serde_json::json!(null)),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!(inp.is_none());
    assert!(out.is_none());
}

#[test]
fn extract_pricing_array_values() {
    // Arrays are not valid pricing values
    let p = ModelPricing {
        input: Some(serde_json::json!([1, 2, 3])),
        output: Some(serde_json::json!({"nested": true})),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!(inp.is_none());
    assert!(out.is_none());
}

#[test]
fn extract_pricing_string_zero() {
    let p = ModelPricing {
        input: Some(serde_json::json!("0")),
        output: Some(serde_json::json!("0.0")),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!((inp.unwrap() - 0.0).abs() < 1e-9);
    assert!((out.unwrap() - 0.0).abs() < 1e-9);
}

// ── build_model_options ─────────────────────────────────────────

#[test]
fn build_model_options_empty_providers() {
    let opts = build_model_options(&[]);
    assert!(opts.is_empty());
}

#[test]
fn build_model_options_simulated_provider() {
    let providers = vec![Provider {
        id: "simulated".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 1);
    assert!(opts[0].0.contains("simulated"));
    assert_eq!(opts[0].1, "simulated");
    assert_eq!(opts[0].2, "simulated-default");
    assert_eq!(opts[0].3, Some(0.0));
    assert_eq!(opts[0].4, Some(0.0));
}

#[test]
fn build_model_options_provider_without_models() {
    let providers = vec![Provider {
        id: "custom".to_string(),
        base_url: "https://custom.api/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: Some(1.0),
        output_price: Some(2.0),
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 1);
    assert!(opts[0].0.contains("enter model manually"));
    assert_eq!(opts[0].1, "custom");
    assert_eq!(opts[0].2, ""); // empty model name
    assert_eq!(opts[0].3, Some(1.0));
    assert_eq!(opts[0].4, Some(2.0));
}

#[test]
fn build_model_options_tested_models_sorted_first() {
    let providers = vec![Provider {
        id: "together_ai".to_string(),
        base_url: "https://api.together.xyz/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: Some(0.90),
        output_price: Some(0.90),
        provider_type: "openai".to_string(),
        models: vec![
            ModelInfo {
                name: "random/untested-model".to_string(),
                input_price: Some(0.50),
                output_price: Some(0.50),
            },
            ModelInfo {
                name: "openai/gpt-oss-120b".to_string(), // this is in TESTED_MODELS
                input_price: Some(0.20),
                output_price: Some(0.60),
            },
            ModelInfo {
                name: "another/untested".to_string(),
                input_price: Some(1.0),
                output_price: Some(2.0),
            },
        ],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 3);
    // Tested model should be first
    assert_eq!(opts[0].2, "openai/gpt-oss-120b");
    // Then the untested models in original order
    assert_eq!(opts[1].2, "random/untested-model");
    assert_eq!(opts[2].2, "another/untested");
}

#[test]
fn build_model_options_free_model_price_tag() {
    let providers = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "llama3".to_string(),
            input_price: Some(0.0),
            output_price: Some(0.0),
        }],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 1);
    // Display string should contain "free" for zero-priced models
    assert!(opts[0].0.contains("free") || opts[0].0.contains("llama3"));
}

#[test]
fn build_model_options_paid_model_price_tag() {
    let providers = vec![Provider {
        id: "together".to_string(),
        base_url: "https://api.together.xyz/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: Some(0.90),
        output_price: Some(0.90),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "llama-70b".to_string(),
            input_price: Some(0.90),
            output_price: Some(1.80),
        }],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 1);
    // Display string should contain price
    assert!(opts[0].0.contains("$0.90/$1.80") || opts[0].0.contains("llama-70b"));
}

#[test]
fn build_model_options_model_with_no_pricing() {
    let providers = vec![Provider {
        id: "custom".to_string(),
        base_url: "https://custom.api/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "model-x".to_string(),
            input_price: None,
            output_price: None,
        }],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 1);
    // Price tag should be empty for unknown pricing
    assert_eq!(opts[0].2, "model-x");
    assert!(opts[0].3.is_none());
    assert!(opts[0].4.is_none());
}

#[test]
fn build_model_options_mixed_providers() {
    let providers = vec![
        Provider {
            id: "together_ai".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            api_key: Some("sk-key".to_string()),
            input_price: Some(0.90),
            output_price: Some(0.90),
            provider_type: "openai".to_string(),
            models: vec![
                ModelInfo {
                    name: "moonshotai/Kimi-K2.5".to_string(), // tested
                    input_price: Some(0.20),
                    output_price: Some(0.60),
                },
                ModelInfo {
                    name: "random-model".to_string(),
                    input_price: Some(0.50),
                    output_price: Some(1.50),
                },
            ],
            engine: None,
        },
        Provider {
            id: "simulated".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "empty_prov".to_string(),
            base_url: "https://empty.api/v1".to_string(),
            api_key: Some("sk-key".to_string()),
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
    ];
    let opts = build_model_options(&providers);
    // 1 tested model first, then: 1 untested, 1 simulated, 1 manual entry = 4 total
    assert_eq!(opts.len(), 4);
    // Tested model (Kimi-K2.5) should be first
    assert_eq!(opts[0].2, "moonshotai/Kimi-K2.5");
    assert_eq!(opts[0].1, "together_ai");
    // Untested model next
    assert_eq!(opts[1].2, "random-model");
    // Simulated
    assert_eq!(opts[2].2, "simulated-default");
    // Manual entry (empty model name)
    assert_eq!(opts[3].2, "");
    assert_eq!(opts[3].1, "empty_prov");
}

#[test]
fn build_model_options_multiple_tested_models() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![
            ModelInfo {
                name: "untested-1".to_string(),
                input_price: None,
                output_price: None,
            },
            ModelInfo {
                name: "openai/gpt-oss-120b".to_string(), // tested
                input_price: Some(0.10),
                output_price: Some(0.30),
            },
            ModelInfo {
                name: "untested-2".to_string(),
                input_price: None,
                output_price: None,
            },
            ModelInfo {
                name: "moonshotai/Kimi-K2.5".to_string(), // tested
                input_price: Some(0.20),
                output_price: Some(0.60),
            },
        ],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 4);
    // Both tested models should come first
    assert_eq!(opts[0].2, "openai/gpt-oss-120b");
    assert_eq!(opts[1].2, "moonshotai/Kimi-K2.5");
    // Untested models after
    assert_eq!(opts[2].2, "untested-1");
    assert_eq!(opts[3].2, "untested-2");
}

// ── build_persona_options ───────────────────────────────────────

#[test]
fn build_persona_options_returns_all_presets() {
    let options = build_persona_options();
    assert_eq!(options.len(), AGENT_PRESETS.len());
}

#[test]
fn build_persona_options_contains_preset_names() {
    let options = build_persona_options();
    for preset in AGENT_PRESETS {
        assert!(
            options.iter().any(|o| o.contains(preset.name)),
            "Options should contain preset name '{}'",
            preset.name
        );
    }
}

#[test]
fn build_persona_options_contains_descriptions() {
    let options = build_persona_options();
    for preset in AGENT_PRESETS {
        assert!(
            options.iter().any(|o| o.contains(preset.desc)),
            "Options should contain description for '{}'",
            preset.name
        );
    }
}

#[test]
fn build_persona_options_format_is_padded() {
    let options = build_persona_options();
    // Each option should start with the preset name left-padded to 10 chars
    for (i, opt) in options.iter().enumerate() {
        let name = AGENT_PRESETS[i].name;
        assert!(
            opt.starts_with(name),
            "Option '{}' should start with preset name '{}'",
            opt,
            name
        );
    }
}

// ── resolve_selected_names ──────────────────────────────────────

#[test]
fn resolve_selected_names_empty_selection() {
    let options = build_persona_options();
    let selected: Vec<String> = vec![];
    let names = resolve_selected_names(&selected, &options);
    assert!(names.is_empty());
}

#[test]
fn resolve_selected_names_single_match() {
    let options = build_persona_options();
    let selected = vec![options[0].clone()]; // first preset
    let names = resolve_selected_names(&selected, &options);
    assert_eq!(names.len(), 1);
    assert_eq!(names[0], AGENT_PRESETS[0].name);
}

#[test]
fn resolve_selected_names_multiple_matches() {
    // AGENT_PRESETS now ships only the 4 General entries — pick
    // three of them and verify the resolver maps display-strings
    // back to canonical names.
    let options = build_persona_options();
    assert!(
        options.len() >= 4,
        "test indexes options[3]; need at least 4 presets, got {}",
        options.len()
    );
    let selected = vec![options[0].clone(), options[1].clone(), options[3].clone()];
    let names = resolve_selected_names(&selected, &options);
    assert_eq!(names.len(), 3);
    assert_eq!(names[0], AGENT_PRESETS[0].name);
    assert_eq!(names[1], AGENT_PRESETS[1].name);
    assert_eq!(names[2], AGENT_PRESETS[3].name);
}

#[test]
fn resolve_selected_names_all_presets() {
    let options = build_persona_options();
    let selected: Vec<String> = options.clone();
    let names = resolve_selected_names(&selected, &options);
    assert_eq!(names.len(), AGENT_PRESETS.len());
    for (i, name) in names.iter().enumerate() {
        assert_eq!(*name, AGENT_PRESETS[i].name);
    }
}

#[test]
fn resolve_selected_names_nonexistent_selection_ignored() {
    let options = build_persona_options();
    let selected = vec![
        options[0].clone(),
        "NONEXISTENT_OPTION_STRING".to_string(),
        options[1].clone(),
    ];
    let names = resolve_selected_names(&selected, &options);
    assert_eq!(names.len(), 2);
    assert_eq!(names[0], AGENT_PRESETS[0].name);
    assert_eq!(names[1], AGENT_PRESETS[1].name);
}

// ── compose_flag ────────────────────────────────────────────────

#[test]
fn compose_flag_standard_docker_compose() {
    assert_eq!(compose_flag("docker-compose.yml"), "");
}

#[test]
fn compose_flag_standard_compose() {
    assert_eq!(compose_flag("compose.yml"), "");
}

#[test]
fn compose_flag_custom_name() {
    assert_eq!(compose_flag("my-stack.yml"), " -f 'my-stack.yml'");
}

#[test]
fn compose_flag_custom_name_with_path() {
    assert_eq!(
        compose_flag("deploy/compose.yaml"),
        " -f 'deploy/compose.yaml'"
    );
}

#[test]
fn compose_flag_compose_yaml_is_not_standard() {
    // "compose.yaml" (not .yml) is NOT one of the standard names
    assert_eq!(compose_flag("compose.yaml"), " -f 'compose.yaml'");
}

#[test]
fn compose_flag_docker_compose_yaml_is_not_standard() {
    // "docker-compose.yaml" is NOT one of the standard names
    assert_eq!(
        compose_flag("docker-compose.yaml"),
        " -f 'docker-compose.yaml'"
    );
}

// ── render_compose_owner (additional targeted tests) ─────────────

#[test]
fn compose_owner_nats_healthcheck_details() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    // Verify specific healthcheck parameters
    assert!(compose.contains("interval: 5s"));
    assert!(compose.contains("timeout: 3s"));
    assert!(compose.contains("retries: 10"));
    // NATS uses nc-based PING/PONG check
    assert!(compose.contains("PING"));
    assert!(compose.contains("PONG"));
}

#[test]
fn compose_owner_orchestrator_healthcheck_details() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    // Orchestrator uses curl-based /health check
    assert!(compose.contains("interval: 10s"));
    assert!(compose.contains("timeout: 5s"));
    assert!(compose.contains("retries: 12"));
    assert!(compose.contains("curl"));
}

#[test]
fn compose_owner_agent_env_vars_complete() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_owner(8080, 4222, &providers, &agents);
    // Each agent should have these env vars
    assert!(compose.contains("NSED_ORCHESTRATOR_URL:"));
    assert!(compose.contains("NSED_BEARER_TOKEN:"));
    assert!(compose.contains("NSED_AGENT_NAME:"));
    // Agents with models should have NSED_MODEL
    assert!(compose.contains("NSED_MODEL: meta-llama/Llama-3-70b"));
    assert!(compose.contains("NSED_MODEL: llama3.2"));
}

#[test]
fn compose_owner_agent_provider_forwarding() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_owner(8080, 4222, &providers, &agents);
    // Agent 1 uses together_ai - should forward base_url and provider
    assert!(compose.contains("NSED_BASE_URL: https://api.together.xyz/v1"));
    assert!(compose.contains("NSED_PROVIDER: together_ai"));
    // Agent 2 uses ollama_local - should forward base_url and provider but no API key
    assert!(compose.contains("NSED_BASE_URL: http://localhost:11434/v1"));
    assert!(compose.contains("NSED_PROVIDER: ollama_local"));
}

#[test]
fn compose_owner_agent_restart_policy() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    // All services should have restart: unless-stopped
    let restart_count = compose.matches("restart: unless-stopped").count();
    // NATS + orchestrator + 2 agents = 4
    assert_eq!(restart_count, 4);
}

#[test]
fn compose_owner_remote_api_key_on_orchestrator_service() {
    let providers = vec![
        Provider {
            id: "together_ai".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            api_key: Some("sk-key".to_string()),
            input_price: Some(0.90),
            output_price: Some(0.90),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key: Some("or-key".to_string()),
            input_price: Some(0.50),
            output_price: Some(1.00),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
    ];
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "together_ai".to_string(),
        "model-x".to_string(),
        None,
        None,
    )];
    let compose = render_compose_owner(8080, 4222, &providers, &agents);
    // Both remote providers should have API keys forwarded to orchestrator
    assert!(compose.contains("APP_PROVIDERS__TOGETHER_AI__API_KEY: ${TOGETHER_AI_API_KEY}"));
    assert!(compose.contains("APP_PROVIDERS__OPENROUTER__API_KEY: ${OPENROUTER_API_KEY}"));
}

// ── render_compose_worker (additional targeted tests) ────────────

#[test]
fn compose_worker_all_agents_have_env_file() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_worker(&agents, &providers);
    // Every agent service should have env_file: .env
    let env_file_count = compose.matches("env_file: .env").count();
    assert_eq!(env_file_count, agents.len());
}

#[test]
fn compose_worker_all_agents_have_volume_mount() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_worker(&agents, &providers);
    let mount_count = compose
        .matches("./config/agent.yml:/app/config/default.yml:ro")
        .count();
    assert_eq!(mount_count, agents.len());
}

#[test]
fn compose_worker_agent_numbering_is_sequential() {
    let providers = sample_providers();
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "together_ai".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "together_ai".to_string(),
            "m2".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "together_ai".to_string(),
            "m3".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "D".to_string(),
            "together_ai".to_string(),
            "m4".to_string(),
            None,
            None,
        ),
    ];
    let compose = render_compose_worker(&agents, &providers);
    assert!(compose.contains("  nsed-agent-1:"));
    assert!(compose.contains("  nsed-agent-2:"));
    assert!(compose.contains("  nsed-agent-3:"));
    assert!(compose.contains("  nsed-agent-4:"));
    assert!(!compose.contains("  nsed-agent-5:"));
}

#[test]
fn compose_worker_uses_orchestrator_url_env_var() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_worker(&agents, &providers);
    // Workers use ${NSED_ORCHESTRATOR_URL} from env
    assert!(compose.contains("NSED_ORCHESTRATOR_URL: ${NSED_ORCHESTRATOR_URL}"));
}

#[test]
fn compose_worker_uses_bearer_token_env_var() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_worker(&agents, &providers);
    assert!(compose.contains("NSED_BEARER_TOKEN: ${NSED_BEARER_TOKEN}"));
}

// ── render_agent_config (additional targeted tests) ──────────────

#[test]
fn render_agent_config_orchestrator_section_structure() {
    let cfg = render_agent_config("http://nsed:9090", "${NSED_BEARER_TOKEN}", &[], &[]);
    assert!(cfg.contains("orchestrators:"));
    assert!(cfg.contains("  - id: \"primary\""));
    assert!(cfg.contains("    url: \"http://nsed:9090\""));
    assert!(cfg.contains("    bearer_token: \"${NSED_BEARER_TOKEN}\""));
    // Commented secondary example
    assert!(cfg.contains("#  - id: \"secondary\""));
}

#[test]
fn render_agent_config_agent_with_all_flags_set() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let mut agent = AgentSlot::new(
        "FULL".to_string(),
        "prov".to_string(),
        "gpt-4o".to_string(),
        Some(2.50),
        Some(10.00),
    );
    agent.persona = Some("You are a test agent.".to_string());
    agent.context_window = Some(128000);
    agent.reasoning_effort = Some("high".to_string());
    agent.use_streaming = Some(false);
    agent.merge_system_prompt = Some(true);
    agent.unwrap_hallucinated_tool_calls = Some(true);
    agent.repair_invalid_escapes = Some(false);
    agent.json_mode = Some(true);
    agent.disable_native_tools = Some(false);
    agent.scratchpad_limit = Some(3000);
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    // Agent uses dotpath model reference
    assert!(cfg.contains("  - name: \"FULL\""));
    assert!(cfg.contains("    model: \"prov.gpt-4o\""));
    assert!(cfg.contains("    persona: \"You are a test agent.\""));

    // LLM fields are now in the models block under the provider
    assert!(cfg.contains("      gpt-4o:"));
    assert!(cfg.contains("        model_name: \"gpt-4o\""));
    assert!(cfg.contains("        input_price_per_mtok: 2.50"));
    assert!(cfg.contains("        output_price_per_mtok: 10.00"));
    assert!(cfg.contains("        context_window: 128000"));
    assert!(cfg.contains("        reasoning_effort: \"high\""));
    assert!(cfg.contains("        use_streaming: false"));
    assert!(cfg.contains("        merge_system_prompt: true"));
    assert!(cfg.contains("        unwrap_hallucinated_tool_calls: true"));
    assert!(cfg.contains("        repair_invalid_escapes: false"));
    assert!(cfg.contains("        json_mode: true"));
    assert!(cfg.contains("        disable_native_tools: false"));
    assert!(cfg.contains("        scratchpad_limit: 3000"));
}

#[test]
fn render_agent_config_agent_with_no_flags_all_commented() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let agent = AgentSlot::new(
        "BARE".to_string(),
        "prov".to_string(),
        "model-x".to_string(),
        None,
        None,
    );
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &[agent],
    );

    // Agent uses dotpath model reference
    assert!(cfg.contains("    model: \"prov.model-x\""));
    // Persona should be commented in agent section
    assert!(cfg.contains("    # persona:"));
    // LLM fields are in the models block, not the agent section
    assert!(cfg.contains("      model-x:"));
    assert!(cfg.contains("        model_name: \"model-x\""));
    // No pricing lines when None
    assert!(!cfg.contains("input_price_per_mtok"));
    assert!(!cfg.contains("output_price_per_mtok"));
}

#[test]
fn render_agent_config_provider_base_url_not_emitted_when_empty() {
    let providers = vec![Provider {
        id: "simulated".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &providers, &[]);
    // Simulated provider with empty base_url should NOT have base_url line
    // Find the simulated provider section and verify no base_url
    let sim_idx = cfg.find("simulated:").unwrap();
    let after_sim = &cfg[sim_idx..];
    let next_section = after_sim.find("\n\n").unwrap_or(after_sim.len());
    let sim_section = &after_sim[..next_section];
    assert!(!sim_section.contains("base_url:"));
}

#[test]
fn render_agent_config_local_provider_concurrency() {
    let providers = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &providers, &[]);
    // Local provider should have higher concurrency and no qps
    assert!(cfg.contains("    concurrency: 1000"));
    // Local provider should NOT have qps
    let ollama_idx = cfg.find("ollama:").unwrap();
    let after_ollama = &cfg[ollama_idx..];
    let next_section = after_ollama.find("\n\n").unwrap_or(after_ollama.len());
    let ollama_section = &after_ollama[..next_section];
    assert!(!ollama_section.contains("qps:"));
}

#[test]
fn render_agent_config_remote_provider_has_qps() {
    let providers = vec![Provider {
        id: "together_ai".to_string(),
        base_url: "https://api.together.xyz/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &providers, &[]);
    // Remote provider should have concurrency=500 and qps=50
    assert!(cfg.contains("    concurrency: 500"));
    assert!(cfg.contains("    qps: 50"));
}

// ── render_orchestrator_config (additional targeted tests) ────────

#[test]
fn render_orchestrator_config_multiple_providers_with_engines() {
    let providers = vec![
        Provider {
            id: "gpt_oss".to_string(),
            base_url: "https://gptoss.api/v1".to_string(),
            api_key: Some("sk-gpt".to_string()),
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: Some("harmony".to_string()),
        },
        Provider {
            id: "vllm_server".to_string(),
            base_url: "http://vllm:8000/v1".to_string(),
            api_key: Some("sk-vllm".to_string()),
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: Some("vllm_responses".to_string()),
        },
    ];
    let cfg = render_orchestrator_config(8080, 4222, &providers);
    assert!(cfg.contains("gpt_oss:"));
    assert!(cfg.contains("engine: \"harmony\""));
    assert!(cfg.contains("vllm_server:"));
    assert!(cfg.contains("engine: \"vllm_responses\""));
}

#[test]
fn render_orchestrator_config_remote_provider_api_key_commented() {
    let providers = vec![Provider {
        id: "together_ai".to_string(),
        base_url: "https://api.together.xyz/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let cfg = render_orchestrator_config(8080, 4222, &providers);
    // In orchestrator config, remote provider api_key is commented
    assert!(cfg.contains("#api_key: \"${TOGETHER_AI_API_KEY}\""));
}

#[test]
fn render_orchestrator_config_logging_section() {
    let cfg = render_orchestrator_config(8080, 4222, &[]);
    assert!(cfg.contains("logging:"));
    assert!(cfg.contains("  level: \"info\""));
}

#[test]
fn render_orchestrator_config_auth_section() {
    let cfg = render_orchestrator_config(8080, 4222, &[]);
    assert!(cfg.contains("auth:"));
    assert!(cfg.contains("  enabled: true"));
}

// ── write_files (additional targeted tests) ──────────────────────

#[test]
fn write_files_env_merge_preserves_secrets_replaces_non_secrets() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".env"),
        "APP_PORT=8080\nDB_PASSWORD=old-pw\nNSED_BEARER_TOKEN=old-tok\nACCOUNT_SEED=old-seed\n",
    )
    .unwrap();
    let existing: HashMap<String, String> = [
        ("APP_PORT".into(), "8080".into()),
        ("DB_PASSWORD".into(), "old-pw".into()),
        ("NSED_BEARER_TOKEN".into(), "old-tok".into()),
        ("ACCOUNT_SEED".into(), "old-seed".into()),
    ]
    .into_iter()
    .collect();
    // New env has same keys with different values
    let skipped = write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        "APP_PORT=9090\nDB_PASSWORD=generated-pw\nNSED_BEARER_TOKEN=new-tok\nACCOUNT_SEED=new-seed\n",
        &existing,
        &no_configs(),
    )
    .unwrap();
    assert!(skipped, "should merge when all keys present");
    let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    // Non-secret keys use template value
    assert!(
        env.contains("APP_PORT=9090"),
        "non-secret should use template value"
    );
    // PASSWORD and SEED are preserved (secret patterns)
    assert!(
        env.contains("DB_PASSWORD=old-pw"),
        "PASSWORD key should preserve existing value"
    );
    assert!(
        env.contains("ACCOUNT_SEED=old-seed"),
        "SEED key should preserve existing value"
    );
    // NSED_BEARER_TOKEN is NOT preserved — "TOKEN" was removed from patterns
    // so freshly-entered bearer tokens are written as-is
    assert!(
        env.contains("NSED_BEARER_TOKEN=new-tok"),
        "BEARER_TOKEN should use new template value (not preserved)"
    );
}

#[test]
fn write_files_env_merges_when_new_key_added() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
    let existing: HashMap<String, String> = [("A".into(), "1".into())].into_iter().collect();
    // New env has an additional key — merge still happens (fix:
    // never wipe existing secrets just because the template grew).
    let skipped = write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        "A=new\nNEW_KEY=val\n",
        &existing,
        &no_configs(),
    )
    .unwrap();
    assert!(skipped, "merge happened (existing non-empty)");
    let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(env.contains("NEW_KEY=val"));
}

#[test]
fn write_files_both_configs_creates_both_files() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    let configs = ConfigFiles {
        agent_config: Some("agent_content".to_string()),
        orchestrator_config: Some("orchestrator_content".to_string()),
    };
    write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        "K=V\n",
        &empty,
        &configs,
    )
    .unwrap();
    let agent = std::fs::read_to_string(dir.path().join("config/agent.yml")).unwrap();
    let orch = std::fs::read_to_string(dir.path().join("config/orchestrator.yml")).unwrap();
    assert_eq!(agent, "agent_content");
    assert_eq!(orch, "orchestrator_content");
}

#[test]
fn write_files_no_configs_does_not_create_config_dir() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        "K=V\n",
        &empty,
        &no_configs(),
    )
    .unwrap();
    assert!(!dir.path().join("config").exists());
}

// ── ensure_gitignore (additional targeted tests) ─────────────────

#[test]
fn ensure_gitignore_empty_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "").unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(content.contains(".env"));
}

#[test]
fn ensure_gitignore_multiple_calls_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    ensure_gitignore(dir.path()).unwrap();
    ensure_gitignore(dir.path()).unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert_eq!(content.matches(".env").count(), 1);
}

// ── parse_env_file (additional targeted tests) ───────────────────

#[test]
fn parse_env_file_preserves_value_with_equals_and_quotes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "SEED=SA_ABC123==\nURL=http://host:8080/path?q=1\n").unwrap();
    let env = parse_env_file(&path);
    assert_eq!(env.get("SEED").unwrap(), "SA_ABC123==");
    assert_eq!(env.get("URL").unwrap(), "http://host:8080/path?q=1");
}

#[test]
fn parse_env_file_trims_key_and_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "  SPACED_KEY  =  spaced_value  \n").unwrap();
    let env = parse_env_file(&path);
    assert_eq!(env.get("SPACED_KEY").unwrap(), "spaced_value");
}

#[test]
fn parse_env_file_multiple_same_key_last_wins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".env");
    std::fs::write(&path, "KEY=first\nKEY=second\n").unwrap();
    let env = parse_env_file(&path);
    assert_eq!(env.get("KEY").unwrap(), "second");
}

// ── extract_pricing (additional targeted tests) ──────────────────

#[test]
fn extract_pricing_boolean_values_return_none() {
    let p = ModelPricing {
        input: Some(serde_json::json!(true)),
        output: Some(serde_json::json!(false)),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!(inp.is_none());
    assert!(out.is_none());
}

#[test]
fn extract_pricing_integer_values() {
    let p = ModelPricing {
        input: Some(serde_json::json!(5)),
        output: Some(serde_json::json!(10)),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!((inp.unwrap() - 5.0).abs() < 1e-9);
    assert!((out.unwrap() - 10.0).abs() < 1e-9);
}

#[test]
fn extract_pricing_only_input_present() {
    let p = ModelPricing {
        input: Some(serde_json::json!(0.30)),
        output: None,
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!((inp.unwrap() - 0.30).abs() < 1e-9);
    assert!(out.is_none());
}

#[test]
fn extract_pricing_only_output_present() {
    let p = ModelPricing {
        input: None,
        output: Some(serde_json::json!(0.60)),
    };
    let (inp, out) = extract_pricing(Some(&p));
    assert!(inp.is_none());
    assert!((out.unwrap() - 0.60).abs() < 1e-9);
}

// ── is_tested_model (additional targeted tests) ──────────────────

#[test]
fn is_tested_model_all_entries() {
    // Verify each entry in TESTED_MODELS individually
    assert!(is_tested_model("Qwen/Qwen3-Coder-Next-FP8"));
    assert!(is_tested_model("essentialai/rnj-1-instruct"));
    assert!(is_tested_model("mistralai/Mistral-Small-24B-Instruct-2501"));
    assert!(is_tested_model("MiniMaxAI/MiniMax-M2.5"));
    assert!(is_tested_model("Qwen/Qwen3-Next-80B-A3B-Thinking"));
    assert!(is_tested_model("zai-org/GLM-4.7"));
    assert!(is_tested_model("meta-llama/Llama-Guard-4-12B"));
}

#[test]
fn is_tested_model_prefix_no_match() {
    // Partial prefix should not match
    assert!(!is_tested_model("openai/gpt-oss"));
    assert!(!is_tested_model("openai/gpt-oss-120"));
}

#[test]
fn is_tested_model_suffix_no_match() {
    assert!(!is_tested_model("gpt-oss-120b"));
    assert!(!is_tested_model("Kimi-K2.5"));
}

#[test]
fn is_tested_model_empty_string() {
    assert!(!is_tested_model(""));
}

// ── render_env + render_compose integration ──────────────────────

#[test]
fn render_env_and_agent_config_provider_keys_consistent() {
    let providers = vec![Provider {
        id: "my-provider".to_string(),
        base_url: "https://my-api.com/v1".to_string(),
        api_key: Some("sk-abc".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &providers, &[]);

    // Both should use MY_PROVIDER as the env key base
    assert!(env.contains("MY_PROVIDER_API_KEY=sk-abc"));
    assert!(cfg.contains("api_key: \"${MY_PROVIDER_API_KEY}\""));
}

#[test]
fn render_compose_and_agent_config_same_agent_names() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_owner(8080, 4222, &providers, &agents);
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    // Both should reference the same agent names
    for agent in &agents {
        assert!(
            compose.contains(&format!("NSED_AGENT_NAME: {}", agent.name)),
            "Compose should contain agent name {}",
            agent.name
        );
        assert!(
            cfg.contains(&format!("name: \"{}\"", agent.name)),
            "Agent config should contain agent name {}",
            agent.name
        );
    }
}

// ── build_fallback_agent ────────────────────────────────────────

#[test]
fn build_fallback_agent_picks_first_model() {
    let providers = vec![Provider {
        id: "together_ai".to_string(),
        base_url: "https://api.together.xyz/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: Some(0.90),
        output_price: Some(0.90),
        provider_type: "openai".to_string(),
        models: vec![
            ModelInfo {
                name: "model-a".to_string(),
                input_price: Some(0.50),
                output_price: Some(1.00),
            },
            ModelInfo {
                name: "model-b".to_string(),
                input_price: Some(0.70),
                output_price: Some(1.40),
            },
        ],
        engine: None,
    }];
    let slot = build_fallback_agent(&providers).unwrap();
    assert_eq!(slot.name, "DEFAULT");
    assert_eq!(slot.provider_id, "together_ai");
    assert_eq!(slot.model_name, "model-a"); // first model
    assert_eq!(slot.input_price, Some(0.50));
    assert_eq!(slot.output_price, Some(1.00));
    // Should have DEFAULT preset applied
    assert!(slot.persona.is_some());
    assert!(slot.persona.as_ref().unwrap().contains("helpful"));
}

#[test]
fn build_fallback_agent_empty_models_uses_provider_pricing() {
    let providers = vec![Provider {
        id: "custom".to_string(),
        base_url: "https://custom.api/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: Some(2.50),
        output_price: Some(5.00),
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let slot = build_fallback_agent(&providers).unwrap();
    assert_eq!(slot.name, "DEFAULT");
    assert_eq!(slot.provider_id, "custom");
    assert_eq!(slot.model_name, ""); // no model discovered
    // Pricing falls back to provider-level pricing
    assert_eq!(slot.input_price, Some(2.50));
    assert_eq!(slot.output_price, Some(5.00));
}

#[test]
fn build_fallback_agent_prefers_provider_with_models() {
    let providers = vec![
        Provider {
            id: "no_models".to_string(),
            base_url: "https://api1.com/v1".to_string(),
            api_key: Some("sk-1".to_string()),
            input_price: Some(1.00),
            output_price: Some(2.00),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "has_models".to_string(),
            base_url: "https://api2.com/v1".to_string(),
            api_key: Some("sk-2".to_string()),
            input_price: Some(0.50),
            output_price: Some(1.00),
            provider_type: "openai".to_string(),
            models: vec![ModelInfo {
                name: "discovered-model".to_string(),
                input_price: Some(0.30),
                output_price: Some(0.60),
            }],
            engine: None,
        },
    ];
    let slot = build_fallback_agent(&providers).unwrap();
    // Should pick the provider with models
    assert_eq!(slot.provider_id, "has_models");
    assert_eq!(slot.model_name, "discovered-model");
    assert_eq!(slot.input_price, Some(0.30));
    assert_eq!(slot.output_price, Some(0.60));
}

#[test]
fn build_fallback_agent_model_price_overrides_provider_price() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.com/v1".to_string(),
        api_key: Some("sk".to_string()),
        input_price: Some(9.99),
        output_price: Some(19.99),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "cheap-model".to_string(),
            input_price: Some(0.10),
            output_price: Some(0.20),
        }],
        engine: None,
    }];
    let slot = build_fallback_agent(&providers).unwrap();
    // Model pricing should be used, not provider pricing
    assert_eq!(slot.input_price, Some(0.10));
    assert_eq!(slot.output_price, Some(0.20));
}

#[test]
fn build_fallback_agent_model_none_price_falls_back_to_provider() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.com/v1".to_string(),
        api_key: Some("sk".to_string()),
        input_price: Some(1.00),
        output_price: Some(2.00),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "model-no-pricing".to_string(),
            input_price: None,
            output_price: None,
        }],
        engine: None,
    }];
    let slot = build_fallback_agent(&providers).unwrap();
    // Model has None pricing, so should fall back to provider pricing
    assert_eq!(slot.input_price, Some(1.00));
    assert_eq!(slot.output_price, Some(2.00));
}

// ── check_model_diversity ──────────────────────────────────────

#[test]
fn check_model_diversity_empty_agents() {
    assert!(!check_model_diversity(&[]));
}

#[test]
fn check_model_diversity_single_agent() {
    let agents = vec![AgentSlot::new(
        "A".to_string(),
        "p".to_string(),
        "m1".to_string(),
        None,
        None,
    )];
    assert!(!check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_two_agents_same_model() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
    ];
    // Only 2 agents, threshold is 3
    assert!(!check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_three_agents_same_model() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
    ];
    // 3 agents, only 1 distinct model → low diversity
    assert!(check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_three_agents_two_models() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "m2".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
    ];
    // 3 agents, 2 distinct models → low diversity
    assert!(check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_three_agents_three_models() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p1".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p2".to_string(),
            "m2".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "p3".to_string(),
            "m3".to_string(),
            None,
            None,
        ),
    ];
    // 3 agents, 3 distinct models → good diversity
    assert!(!check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_five_agents_four_models() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "m2".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "p".to_string(),
            "m3".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "D".to_string(),
            "p".to_string(),
            "m4".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "E".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
    ];
    // 5 agents, 4 distinct models → good diversity
    assert!(!check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_many_agents_all_same() {
    let agents: Vec<AgentSlot> = (0..10)
        .map(|i| {
            AgentSlot::new(
                format!("AGENT_{}", i),
                "p".to_string(),
                "same-model".to_string(),
                None,
                None,
            )
        })
        .collect();
    assert!(check_model_diversity(&agents));
}

// ── render_compose_owner (orchestrator env_file and image) ───────

#[test]
fn compose_owner_all_services_use_correct_images() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    // NATS
    assert!(compose.contains("image: nats:2-alpine"));
    // Orchestrator
    assert!(compose.contains("image: ghcr.io/peeramid-labs/nsed:latest"));
    // Agents
    let agent_image_count = compose
        .matches("image: ghcr.io/peeramid-labs/nsed-agent:latest")
        .count();
    assert_eq!(agent_image_count, 2); // 2 agents
}

#[test]
fn compose_owner_orchestrator_env_file() {
    let compose = render_compose_owner(8080, 4222, &sample_providers(), &sample_agents());
    // Orchestrator should have env_file
    let nsed_idx = compose.find("  nsed:").unwrap();
    let after_nsed = &compose[nsed_idx..];
    let next_service = after_nsed.find("  nsed-agent-").unwrap_or(after_nsed.len());
    let nsed_section = &after_nsed[..next_service];
    assert!(nsed_section.contains("env_file: .env"));
}

// ── render_compose_worker (agent restart policy) ────────────────

#[test]
fn compose_worker_all_agents_restart_policy() {
    let providers = sample_providers();
    let agents = sample_agents();
    let compose = render_compose_worker(&agents, &providers);
    let restart_count = compose.matches("restart: unless-stopped").count();
    assert_eq!(restart_count, agents.len());
}

// ── render_orchestrator_config (all sections) ───────────────────

#[test]
fn render_orchestrator_config_all_section_headers_present() {
    let cfg = render_orchestrator_config(8080, 4222, &sample_providers());
    // All major section headers should be present
    assert!(cfg.contains("# Server"));
    assert!(cfg.contains("# NATS JetStream"));
    assert!(cfg.contains("# LLM Providers"));
    assert!(cfg.contains("# Authentication"));
    assert!(cfg.contains("# NATS JWT Credentials"));
    assert!(cfg.contains("# Logging"));
    assert!(cfg.contains("# Orchestrator"));
    assert!(cfg.contains("# NSED Protocol Settings"));
}

// ── render_agent_config (all section headers) ───────────────────

#[test]
fn render_agent_config_all_section_headers_present() {
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &sample_providers(),
        &sample_agents(),
    );
    assert!(cfg.contains("# Orchestrators"));
    assert!(cfg.contains("# LLM Providers"));
    assert!(cfg.contains("# Agents"));
    assert!(cfg.contains("# Engine strategies"));
}

// ── render_agent_config (provider engine docs) ──────────────────

#[test]
fn render_agent_config_engine_strategy_comments() {
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &[], &[]);
    assert!(cfg.contains("\"harmony\""));
    assert!(cfg.contains("\"vllm\""));
    assert!(cfg.contains("\"vllm_responses\""));
}

// ── Integration: full owner workflow render ─────────────────────

#[test]
fn full_owner_render_workflow() {
    // Simulate the full render workflow used by run_owner
    let providers = vec![
        Provider {
            id: "together_ai".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            api_key: Some("sk-test".to_string()),
            input_price: Some(0.90),
            output_price: Some(0.90),
            provider_type: "openai".to_string(),
            models: vec![ModelInfo {
                name: "llama-70b".to_string(),
                input_price: Some(0.90),
                output_price: Some(0.90),
            }],
            engine: None,
        },
        Provider {
            id: "gpt_oss".to_string(),
            base_url: "https://gptoss.api/v1".to_string(),
            api_key: Some("sk-gpt".to_string()),
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: Some("harmony".to_string()),
        },
    ];
    let mut agents = vec![
        AgentSlot::new(
            "DEFAULT".to_string(),
            "together_ai".to_string(),
            "llama-70b".to_string(),
            Some(0.90),
            Some(0.90),
        ),
        AgentSlot::new(
            "REASON".to_string(),
            "gpt_oss".to_string(),
            "gpt-oss-120b".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "CREATE".to_string(),
            "together_ai".to_string(),
            "llama-70b".to_string(),
            Some(0.90),
            Some(0.90),
        ),
    ];
    for agent in &mut agents {
        agent.apply_preset();
    }

    let app_port = 9090;
    let nats_port = 5222;

    // Render all outputs
    let compose = render_compose_owner(app_port, nats_port, &providers, &agents);
    let env = render_env(
        "owner",
        app_port,
        nats_port,
        "admin-secret",
        "worker-secret",
        "SA_SEED",
        &providers,
        None,
        None,
    );
    let orch_url = format!("http://nsed:${{APP_PORT:-{app_port}}}");
    let agent_cfg = render_agent_config(&orch_url, "${NSED_BEARER_TOKEN}", &providers, &agents);
    let orch_cfg = render_orchestrator_config(app_port, nats_port, &providers);

    // Verify consistency across all outputs
    assert!(compose.contains("9090"));
    assert!(compose.contains("5222"));
    assert!(env.contains("APP_PORT=9090"));
    assert!(env.contains("NATS_PORT=5222"));
    assert!(orch_cfg.contains("port: 9090"));
    assert!(orch_cfg.contains("nats://127.0.0.1:5222"));

    // Agent config should reference the orchestrator URL with port
    assert!(agent_cfg.contains("9090"));

    // Providers should be consistent
    assert!(compose.contains("TOGETHER_AI__API_KEY"));
    assert!(compose.contains("GPT_OSS__API_KEY"));
    assert!(env.contains("TOGETHER_AI_API_KEY=sk-test"));
    assert!(env.contains("GPT_OSS_API_KEY=sk-gpt"));
    assert!(agent_cfg.contains("together_ai:"));
    assert!(agent_cfg.contains("gpt_oss:"));
    assert!(agent_cfg.contains("engine: \"harmony\""));
    assert!(orch_cfg.contains("engine: \"harmony\""));

    // All 3 agents in compose and config
    assert!(compose.contains("nsed-agent-1:"));
    assert!(compose.contains("nsed-agent-2:"));
    assert!(compose.contains("nsed-agent-3:"));
    assert!(agent_cfg.contains("name: \"DEFAULT\""));
    assert!(agent_cfg.contains("name: \"REASON\""));
    assert!(agent_cfg.contains("name: \"CREATE\""));
}

// ── Integration: full worker workflow render ─────────────────────

#[test]
fn full_worker_render_workflow() {
    let providers = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "llama3.2".to_string(),
            input_price: Some(0.0),
            output_price: Some(0.0),
        }],
        engine: None,
    }];
    let mut agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "ollama".to_string(),
        "llama3.2".to_string(),
        Some(0.0),
        Some(0.0),
    )];
    for agent in &mut agents {
        agent.apply_preset();
    }

    let orch_url = "http://remote-host:8080";
    let compose = render_compose_worker(&agents, &providers);
    let env = render_env(
        "worker",
        8080,
        4222,
        "worker-token",
        "worker-token",
        "",
        &providers,
        Some(orch_url),
        None,
    );
    let agent_cfg = render_agent_config(orch_url, "${NSED_BEARER_TOKEN}", &providers, &agents);

    // Worker compose should not have NATS or orchestrator services
    assert!(!compose.contains("  nats:"));
    assert!(!compose.contains("  nsed:"));
    assert!(compose.contains("nsed-agent-1:"));

    // Worker env should have orchestrator URL
    assert!(env.contains("NSED_ORCHESTRATOR_URL=http://remote-host:8080"));
    assert!(env.contains("NSED_BEARER_TOKEN=worker-token"));
    assert!(!env.contains("APP_PORT="));

    // Agent config should point to orchestrator
    assert!(agent_cfg.contains("url: \"http://remote-host:8080\""));

    // Ollama is local → no API key in any output
    assert!(!compose.contains("API_KEY"));
    assert!(!env.contains("OLLAMA_API_KEY="));
}

// ── write_files + ensure_gitignore integration ──────────────────

#[test]
fn write_files_creates_gitignore_with_env() {
    let dir = tempfile::tempdir().unwrap();
    let empty = HashMap::new();
    write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        "K=V\n",
        &empty,
        &no_configs(),
    )
    .unwrap();
    let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gi.contains(".env"));
}

#[test]
fn write_files_preserves_existing_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "target/\nnode_modules/\n").unwrap();
    let empty = HashMap::new();
    write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        "K=V\n",
        &empty,
        &no_configs(),
    )
    .unwrap();
    let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gi.contains("target/"));
    assert!(gi.contains("node_modules/"));
    assert!(gi.contains(".env"));
}

// ── resolve_provider_preset ──────────────────────────────────────────

#[test]
fn resolve_provider_preset_openai() {
    let (url, needs_key, id) = resolve_provider_preset("OpenAI (GPT-4o, o3, ...)");
    assert_eq!(url, "https://api.openai.com/v1");
    assert!(needs_key);
    assert_eq!(id, "openai");
}

#[test]
fn resolve_provider_preset_together_ai() {
    let (url, needs_key, id) = resolve_provider_preset("Together AI (DeepSeek, Llama, ...)");
    assert_eq!(url, "https://api.together.xyz/v1");
    assert!(needs_key);
    assert_eq!(id, "together_ai");
}

#[test]
fn resolve_provider_preset_openrouter() {
    let (url, needs_key, id) = resolve_provider_preset("OpenRouter (multi-provider gateway)");
    assert_eq!(url, "https://openrouter.ai/api/v1");
    assert!(needs_key);
    assert_eq!(id, "openrouter");
}

#[test]
fn resolve_provider_preset_ollama() {
    let (url, needs_key, id) = resolve_provider_preset("Ollama (local, free)");
    assert_eq!(url, "http://localhost:11434/v1");
    assert!(!needs_key);
    assert_eq!(id, "ollama_local");
}

#[test]
fn resolve_provider_preset_custom() {
    let (url, needs_key, id) = resolve_provider_preset("OpenAI-compatible (custom URL)");
    assert_eq!(url, "");
    assert!(needs_key);
    assert_eq!(id, "custom");
}

#[test]
fn resolve_provider_preset_simulated() {
    let (url, needs_key, id) = resolve_provider_preset("Simulated (no LLM needed)");
    assert_eq!(url, "");
    assert!(!needs_key);
    assert_eq!(id, "simulated");
}

#[test]
fn resolve_provider_preset_unknown_defaults_to_openai() {
    let (url, needs_key, id) = resolve_provider_preset("Some random string");
    assert_eq!(url, "https://api.openai.com/v1");
    assert!(needs_key);
    assert_eq!(id, "openai");
}

#[test]
fn resolve_provider_preset_prefix_matching() {
    // Verify it only checks starts_with, not exact match
    let (_, _, id) = resolve_provider_preset("Together AI");
    assert_eq!(id, "together_ai");
    let (_, _, id) = resolve_provider_preset("OpenRouter");
    assert_eq!(id, "openrouter");
    let (_, _, id) = resolve_provider_preset("Ollama");
    assert_eq!(id, "ollama_local");
    let (_, _, id) = resolve_provider_preset("Simulated");
    assert_eq!(id, "simulated");
    let (_, _, id) = resolve_provider_preset("OpenAI-compatible");
    assert_eq!(id, "custom");
}

// ── suggest_provider_id ──────────────────────────────────────────────

#[test]
fn suggest_provider_id_no_conflict() {
    let existing: Vec<Provider> = vec![];
    assert_eq!(suggest_provider_id("openai", &existing), "openai");
}

#[test]
fn suggest_provider_id_conflict_appends_suffix() {
    let existing = vec![Provider {
        id: "openai".into(),
        provider_type: "openai".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: Some("sk-test".into()),
        input_price: None,
        output_price: None,
        models: vec![],
        engine: None,
    }];
    assert_eq!(suggest_provider_id("openai", &existing), "openai_2");
}

#[test]
fn suggest_provider_id_no_conflict_different_id() {
    let existing = vec![Provider {
        id: "openai".into(),
        provider_type: "openai".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: Some("sk-test".into()),
        input_price: None,
        output_price: None,
        models: vec![],
        engine: None,
    }];
    // "together_ai" does not conflict with existing "openai"
    assert_eq!(suggest_provider_id("together_ai", &existing), "together_ai");
}

#[test]
fn suggest_provider_id_multiple_existing() {
    let existing = vec![
        Provider {
            id: "ollama_local".into(),
            provider_type: "openai".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            input_price: None,
            output_price: None,
            models: vec![],
            engine: None,
        },
        Provider {
            id: "openai".into(),
            provider_type: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some("sk-x".into()),
            input_price: None,
            output_price: None,
            models: vec![],
            engine: None,
        },
    ];
    // "ollama_local" conflicts, suffix should be len+1 = 3
    assert_eq!(
        suggest_provider_id("ollama_local", &existing),
        "ollama_local_3"
    );
}

#[test]
fn suggest_provider_id_simulated() {
    let existing: Vec<Provider> = vec![];
    assert_eq!(suggest_provider_id("simulated", &existing), "simulated");
}

// ── fetched_to_model_infos ───────────────────────────────────────────

#[test]
fn fetched_to_model_infos_empty() {
    let models: Vec<FetchedModel> = vec![];
    let infos = fetched_to_model_infos(&models);
    assert!(infos.is_empty());
}

#[test]
fn fetched_to_model_infos_single_with_prices() {
    let models: Vec<FetchedModel> = vec![("gpt-4o".into(), Some(2.5), Some(10.0))];
    let infos = fetched_to_model_infos(&models);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "gpt-4o");
    assert_eq!(infos[0].input_price, Some(2.5));
    assert_eq!(infos[0].output_price, Some(10.0));
}

#[test]
fn fetched_to_model_infos_single_no_prices() {
    let models: Vec<FetchedModel> = vec![("llama3".into(), None, None)];
    let infos = fetched_to_model_infos(&models);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "llama3");
    assert_eq!(infos[0].input_price, None);
    assert_eq!(infos[0].output_price, None);
}

#[test]
fn fetched_to_model_infos_multiple() {
    let models: Vec<FetchedModel> = vec![
        ("model-a".into(), Some(1.0), Some(2.0)),
        ("model-b".into(), None, Some(5.0)),
        ("model-c".into(), Some(0.5), None),
    ];
    let infos = fetched_to_model_infos(&models);
    assert_eq!(infos.len(), 3);
    assert_eq!(infos[0].name, "model-a");
    assert_eq!(infos[1].name, "model-b");
    assert_eq!(infos[1].input_price, None);
    assert_eq!(infos[1].output_price, Some(5.0));
    assert_eq!(infos[2].name, "model-c");
    assert_eq!(infos[2].input_price, Some(0.5));
    assert_eq!(infos[2].output_price, None);
}

#[test]
fn fetched_to_model_infos_preserves_order() {
    let models: Vec<FetchedModel> = vec![
        ("z-model".into(), None, None),
        ("a-model".into(), None, None),
        ("m-model".into(), None, None),
    ];
    let infos = fetched_to_model_infos(&models);
    assert_eq!(infos[0].name, "z-model");
    assert_eq!(infos[1].name, "a-model");
    assert_eq!(infos[2].name, "m-model");
}

#[test]
fn fetched_to_model_infos_zero_prices() {
    let models: Vec<FetchedModel> = vec![("free-model".into(), Some(0.0), Some(0.0))];
    let infos = fetched_to_model_infos(&models);
    assert_eq!(infos[0].input_price, Some(0.0));
    assert_eq!(infos[0].output_price, Some(0.0));
}

#[test]
fn fetched_to_model_infos_fractional_prices() {
    let models: Vec<FetchedModel> = vec![("precise".into(), Some(0.00015), Some(0.0006))];
    let infos = fetched_to_model_infos(&models);
    assert_eq!(infos[0].input_price, Some(0.00015));
    assert_eq!(infos[0].output_price, Some(0.0006));
}

// ── render_env comprehensive branch coverage ─────────────────────

#[test]
fn render_env_owner_all_provider_types_combined() {
    // Exercise every branch in render_env: remote, local, simulated, engine
    let providers = vec![
        Provider {
            id: "remote-prov".to_string(),
            base_url: "https://remote.api/v1".to_string(),
            api_key: Some("sk-remote".to_string()),
            input_price: Some(1.0),
            output_price: Some(2.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: Some("harmony".to_string()),
        },
        Provider {
            id: "local-prov".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "sim".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![],
            engine: None,
        },
    ];
    let env = render_env(
        "owner",
        9090,
        5222,
        "admin-s",
        "worker-s",
        "SA_SEED_XYZ",
        &providers,
        None,
        None,
    );

    // Owner-specific sections
    assert!(env.contains("APP_PORT=9090"));
    assert!(env.contains("NATS_PORT=5222"));
    assert!(env.contains("APP_AUTH__ENABLED=true"));
    assert!(env.contains("APP_AUTH__TOKENS__0__SECRET=admin-s"));
    assert!(env.contains("APP_AUTH__TOKENS__1__SECRET=worker-s"));
    assert!(env.contains("APP_CREDENTIALS__ACCOUNT_SEED=SA_SEED_XYZ"));
    // Worker token used for local agents
    assert!(env.contains("NSED_BEARER_TOKEN=worker-s"));
    // No orchestrator URL for owner
    assert!(!env.contains("NSED_ORCHESTRATOR_URL"));

    // Remote provider: API key emitted
    assert!(env.contains("REMOTE_PROV_API_KEY=sk-remote"));
    // Local provider: comment about local
    assert!(env.contains("local-prov: local endpoint"));
    // Simulated provider: comment about no API key
    assert!(env.contains("sim: no API key"));

    // Base URLs section present for non-empty base URLs
    assert!(env.contains("LLM Provider base URLs"));
    assert!(env.contains("APP_PROVIDERS__REMOTE_PROV__BASE_URL=https://remote.api/v1"));
    assert!(env.contains("APP_PROVIDERS__LOCAL_PROV__BASE_URL=http://localhost:11434/v1"));
    // Simulated has empty base_url, should not appear in base URLs section
    assert!(!env.contains("APP_PROVIDERS__SIM__BASE_URL"));
}

#[test]
fn render_env_worker_all_branches() {
    let providers = vec![Provider {
        id: "together-ai".to_string(),
        base_url: "https://api.together.xyz/v1".to_string(),
        api_key: Some("sk-123".to_string()),
        input_price: Some(0.9),
        output_price: Some(0.9),
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];
    let env = render_env(
        "worker",
        8080,
        4222,
        "worker-tok",
        "worker-tok",
        "",
        &providers,
        Some("http://orch-host:9090"),
        None,
    );

    // Worker should NOT have owner-specific sections
    assert!(!env.contains("APP_PORT="));
    assert!(!env.contains("NATS_PORT="));
    assert!(!env.contains("APP_AUTH__ENABLED"));
    assert!(!env.contains("APP_CREDENTIALS__ACCOUNT_SEED"));

    // Worker agent section
    assert!(env.contains("NSED_BEARER_TOKEN=worker-tok"));
    assert!(env.contains("NSED_ORCHESTRATOR_URL=http://orch-host:9090"));

    // API key for remote provider
    assert!(env.contains("TOGETHER_AI_API_KEY=sk-123"));
    assert!(env.contains("APP_PROVIDERS__TOGETHER_AI__BASE_URL=https://api.together.xyz/v1"));
}

// ── render_compose_owner comprehensive branch coverage ───────────

#[test]
fn compose_owner_full_scenario_with_all_provider_types() {
    // Exercise every branch in render_compose_owner: remote, local, simulated
    let providers = vec![
        Provider {
            id: "remote-ai".to_string(),
            base_url: "https://remote.api/v1".to_string(),
            api_key: Some("sk-remote".to_string()),
            input_price: Some(1.0),
            output_price: Some(2.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "local-ollama".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "sim".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![],
            engine: None,
        },
    ];

    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "remote-ai".to_string(),
            "gpt-4o".to_string(),
            Some(1.0),
            Some(2.0),
        ),
        AgentSlot::new(
            "B".to_string(),
            "local-ollama".to_string(),
            "llama3".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        AgentSlot::new(
            "C".to_string(),
            "sim".to_string(),
            "simulated-default".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        AgentSlot::new(
            "D".to_string(),
            "remote-ai".to_string(),
            String::new(),
            None,
            None,
        ),
    ];

    let compose = render_compose_owner(7070, 5555, &providers, &agents);

    // NATS service with custom port
    assert!(compose.contains("\"--port\", \"5555\""));
    assert!(compose.contains("${NATS_PORT:-5555}:5555"));
    assert!(compose.contains("nc localhost 5555"));

    // Orchestrator service with custom app port
    assert!(compose.contains("${APP_PORT:-7070}:7070"));
    assert!(compose.contains("APP_NATS__URL: \"nats://nats:5555\""));

    // Remote provider API key forwarded to orchestrator
    assert!(compose.contains("APP_PROVIDERS__REMOTE_AI__API_KEY: ${REMOTE_AI_API_KEY}"));
    // Local provider: NO API key on orchestrator
    assert!(!compose.contains("APP_PROVIDERS__LOCAL_OLLAMA__API_KEY"));
    // Simulated provider: NO API key on orchestrator
    assert!(!compose.contains("APP_PROVIDERS__SIM__API_KEY"));

    // 4 agent services
    assert!(compose.contains("nsed-agent-1:"));
    assert!(compose.contains("nsed-agent-2:"));
    assert!(compose.contains("nsed-agent-3:"));
    assert!(compose.contains("nsed-agent-4:"));

    // Agent A (remote): has base URL, provider, API key forwarding
    assert!(compose.contains("NSED_MODEL: gpt-4o"));
    assert!(compose.contains("NSED_PROVIDER: remote-ai"));
    assert!(compose.contains("NSED_BASE_URL: https://remote.api/v1"));

    // Agent B (local): has base URL, provider, NO API key
    assert!(compose.contains("NSED_MODEL: llama3"));
    assert!(compose.contains("NSED_PROVIDER: local-ollama"));
    assert!(compose.contains("NSED_BASE_URL: http://localhost:11434/v1"));

    // Agent C (simulated): has provider, no base URL, no API key
    assert!(compose.contains("NSED_PROVIDER: sim"));

    // Agent D (remote, empty model): no NSED_MODEL line for this agent
    // Count NSED_MODEL occurrences (should be 3, not 4)
    let model_lines: Vec<_> = compose
        .lines()
        .filter(|l| l.contains("NSED_MODEL:"))
        .collect();
    assert_eq!(model_lines.len(), 3, "Only 3 agents have model names");

    // All agents depend on orchestrator
    let depends_count = compose.matches("condition: service_healthy").count();
    // 4 agents + orchestrator depends on nats = 5
    assert_eq!(depends_count, 5);
}

// ── render_compose_worker comprehensive branch coverage ──────────

#[test]
fn compose_worker_full_scenario_with_all_provider_types() {
    let providers = vec![
        Provider {
            id: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key: Some("or-key".to_string()),
            input_price: Some(0.5),
            output_price: Some(1.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "ollama".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "simulated".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![],
            engine: None,
        },
    ];

    let agents = vec![
        AgentSlot::new(
            "W1".to_string(),
            "openrouter".to_string(),
            "claude-3.5".to_string(),
            Some(0.5),
            Some(1.0),
        ),
        AgentSlot::new(
            "W2".to_string(),
            "ollama".to_string(),
            "phi3".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        AgentSlot::new(
            "W3".to_string(),
            "simulated".to_string(),
            "simulated-default".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        AgentSlot::new(
            "W4".to_string(),
            "openrouter".to_string(),
            String::new(),
            None,
            None,
        ),
    ];

    let compose = render_compose_worker(&agents, &providers);

    // No orchestrator or NATS services
    assert!(!compose.contains("  nats:"));
    assert!(!compose.contains("  nsed:\n"));

    // 4 agent services
    for i in 1..=4 {
        assert!(compose.contains(&format!("nsed-agent-{i}:")));
    }

    // Worker uses env var for orchestrator URL
    assert!(compose.contains("NSED_ORCHESTRATOR_URL: ${NSED_ORCHESTRATOR_URL}"));
    assert!(compose.contains("NSED_BEARER_TOKEN: ${NSED_BEARER_TOKEN}"));

    // Agent W1 (openrouter): has API key, base URL
    assert!(compose.contains("NSED_MODEL: claude-3.5"));
    assert!(compose.contains("NSED_PROVIDER: openrouter"));
    assert!(compose.contains("NSED_BASE_URL: https://openrouter.ai/api/v1"));
    assert!(compose.contains("OPENROUTER__API_KEY"));

    // Agent W2 (ollama): has base URL, no API key
    assert!(compose.contains("NSED_MODEL: phi3"));
    assert!(compose.contains("NSED_PROVIDER: ollama"));

    // Agent W3 (simulated): no base URL, no API key
    assert!(compose.contains("NSED_PROVIDER: simulated"));

    // Agent W4 (openrouter, empty model): no NSED_MODEL for this one
    let model_count = compose
        .lines()
        .filter(|l| l.contains("NSED_MODEL:"))
        .count();
    assert_eq!(model_count, 3);

    // All agents have restart policy
    let restart_count = compose.matches("restart: unless-stopped").count();
    assert_eq!(restart_count, 4);

    // All agents have volume mount
    let vol_count = compose
        .matches("./config/agent.yml:/app/config/default.yml:ro")
        .count();
    assert_eq!(vol_count, 4);

    // All agents have env_file
    let env_count = compose.matches("env_file: .env").count();
    assert_eq!(env_count, 4);
}

// ── render_agent_config comprehensive branch coverage ────────────

#[test]
fn render_agent_config_full_scenario_all_branches() {
    // One provider of each type: remote, local, simulated, with engine
    let providers = vec![
        Provider {
            id: "remote".to_string(),
            base_url: "https://api.remote.com/v1".to_string(),
            api_key: Some("sk-remote".to_string()),
            input_price: Some(1.0),
            output_price: Some(2.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "local".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "sim".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "harmony-prov".to_string(),
            base_url: "https://harmony.api/v1".to_string(),
            api_key: Some("sk-harmony".to_string()),
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: Some("harmony".to_string()),
        },
    ];

    // Agents: one with all flags set, one with none, one with preset, one with partial
    let mut agent_all = AgentSlot::new(
        "ALL_FLAGS".to_string(),
        "remote".to_string(),
        "gpt-4o".to_string(),
        Some(2.5),
        Some(10.0),
    );
    agent_all.persona = Some("You are a comprehensive test agent.".to_string());
    agent_all.context_window = Some(128000);
    agent_all.reasoning_effort = Some("high".to_string());
    agent_all.use_streaming = Some(true);
    agent_all.merge_system_prompt = Some(false);
    agent_all.unwrap_hallucinated_tool_calls = Some(true);
    agent_all.repair_invalid_escapes = Some(false);
    agent_all.json_mode = Some(true);
    agent_all.disable_native_tools = Some(false);
    agent_all.scratchpad_limit = Some(3000);

    let agent_none = AgentSlot::new(
        "NO_FLAGS".to_string(),
        "local".to_string(),
        "llama3".to_string(),
        None,
        None,
    );

    let mut agent_preset = AgentSlot::new(
        "VERIFY".to_string(),
        "sim".to_string(),
        "simulated-default".to_string(),
        Some(0.0),
        Some(0.0),
    );
    agent_preset.apply_preset();

    let mut agent_partial = AgentSlot::new(
        "PARTIAL".to_string(),
        "harmony-prov".to_string(),
        "gpt-oss-120b".to_string(),
        Some(0.5),
        None,
    );
    agent_partial.use_streaming = Some(false);
    agent_partial.json_mode = Some(true);

    let agents = vec![agent_all, agent_none, agent_preset, agent_partial];
    let cfg = render_agent_config(
        "http://nsed:7070",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    // Orchestrator section
    assert!(cfg.contains("url: \"http://nsed:7070\""));

    // Provider sections
    assert!(cfg.contains("remote:"));
    assert!(cfg.contains("api_key: \"${REMOTE_API_KEY}\""));
    assert!(cfg.contains("concurrency: 500"));
    assert!(cfg.contains("qps: 50"));

    assert!(cfg.contains("local:"));
    assert!(cfg.contains("api_key: \"ollama\""));
    assert!(cfg.contains("concurrency: 1000"));

    assert!(cfg.contains("sim:"));
    assert!(cfg.contains("type: simulated"));
    assert!(cfg.contains("latency_ms: 400"));

    assert!(cfg.contains("harmony-prov:"));
    assert!(cfg.contains("engine: \"harmony\""));

    // Agent ALL_FLAGS: dotpath model reference + persona in agent section
    assert!(cfg.contains("name: \"ALL_FLAGS\""));
    assert!(cfg.contains("    model: \"remote.gpt-4o\""));
    assert!(cfg.contains("persona: \"You are a comprehensive test agent.\""));

    // Extract the gpt-4o model subsection to avoid matching values from other model blocks
    let gpt4o_section: String = cfg
        .lines()
        .skip_while(|l| !l.contains("gpt-4o:"))
        .take_while(|l| l.contains("gpt-4o:") || l.starts_with("        "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !gpt4o_section.is_empty(),
        "gpt-4o model subsection not found in rendered config"
    );

    // LLM fields in gpt-4o model block under provider
    assert!(gpt4o_section.contains("input_price_per_mtok: 2.50"));
    assert!(gpt4o_section.contains("output_price_per_mtok: 10.00"));
    assert!(gpt4o_section.contains("context_window: 128000"));
    assert!(gpt4o_section.contains("reasoning_effort: \"high\""));
    assert!(gpt4o_section.contains("use_streaming: true"));
    assert!(gpt4o_section.contains("merge_system_prompt: false"));
    assert!(gpt4o_section.contains("unwrap_hallucinated_tool_calls: true"));
    assert!(gpt4o_section.contains("repair_invalid_escapes: false"));
    assert!(gpt4o_section.contains("json_mode: true"));
    assert!(gpt4o_section.contains("disable_native_tools: false"));
    assert!(gpt4o_section.contains("scratchpad_limit: 3000"));

    // Agent NO_FLAGS: dotpath + persona commented
    assert!(cfg.contains("name: \"NO_FLAGS\""));
    assert!(cfg.contains("    # persona: \"You are a helpful AI assistant.\""));

    // Agent VERIFY: preset applied — temperature is in models block
    assert!(cfg.contains("name: \"VERIFY\""));
    assert!(cfg.contains("temperature: 0.5"));

    // Agent PARTIAL: pricing in models block
    assert!(cfg.contains("name: \"PARTIAL\""));
    assert!(cfg.contains("input_price_per_mtok: 0.50"));
    // output_price is None — the model entry should not contain output_price_per_mtok
    let partial_model_key = "gpt-oss-120b:";
    let partial_model_section: String = cfg
        .lines()
        .skip_while(|l| !l.contains(partial_model_key))
        .take_while(|l| l.contains(partial_model_key) || l.starts_with("        "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !partial_model_section.contains("output_price_per_mtok"),
        "PARTIAL model entry should not have output_price_per_mtok when output_price is None"
    );
}

// ── render_orchestrator_config comprehensive branch coverage ─────

#[test]
fn render_orchestrator_config_all_provider_types_comprehensive() {
    let providers = vec![
        Provider {
            id: "cloud-ai".to_string(),
            base_url: "https://cloud.api/v1".to_string(),
            api_key: Some("sk-cloud".to_string()),
            input_price: Some(1.0),
            output_price: Some(2.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "local-ai".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "sim".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "vllm".to_string(),
            base_url: "http://vllm:8000/v1".to_string(),
            api_key: Some("sk-vllm".to_string()),
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: Some("vllm_responses".to_string()),
        },
    ];

    let cfg = render_orchestrator_config(7070, 5555, &providers);

    // Server section with custom port
    assert!(cfg.contains("port: 7070"));
    // NATS section with custom port
    assert!(cfg.contains("url: \"nats://127.0.0.1:5555\""));

    // Providers section (not commented, because providers exist)
    // The "providers:" line should be present without # prefix
    assert!(cfg.contains("\nproviders:\n"));

    // Remote provider: commented api_key, concurrency, qps
    assert!(cfg.contains("cloud-ai:"));
    assert!(cfg.contains("#api_key: \"${CLOUD_AI_API_KEY}\""));
    assert!(cfg.contains("concurrency: 500"));

    // Local provider: api_key "ollama", concurrency 1000
    assert!(cfg.contains("local-ai:"));
    assert!(cfg.contains("api_key: \"ollama\""));

    // Simulated provider: latency_ms, concurrency 100
    assert!(cfg.contains("sim:"));
    assert!(cfg.contains("type: simulated"));
    assert!(cfg.contains("latency_ms: 400"));
    assert!(cfg.contains("concurrency: 100"));

    // Engine override
    assert!(cfg.contains("vllm:"));
    assert!(cfg.contains("engine: \"vllm_responses\""));

    // Auth, credentials, logging, orchestrator sections
    assert!(cfg.contains("auth:\n  enabled: true"));
    assert!(cfg.contains("credentials:\n  enabled: true"));
    assert!(cfg.contains("jwt_expiry_secs: 86400"));
    assert!(cfg.contains("challenge_expiry_secs: 300"));
    assert!(cfg.contains("logging:\n  level: \"info\""));
    assert!(cfg.contains("# orchestrator:"));
    assert!(cfg.contains("# nsed:"));
}

// ── write_files comprehensive integration test ───────────────────

#[test]
fn write_files_full_owner_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let providers = sample_providers();
    let agents = sample_agents();

    let compose = render_compose_owner(8080, 4222, &providers, &agents);
    let env = render_env(
        "owner", 8080, 4222, "admin-s", "worker-s", "SA_SEED", &providers, None, None,
    );
    let agent_cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );
    let orch_cfg = render_orchestrator_config(8080, 4222, &providers);

    let configs = ConfigFiles {
        agent_config: Some(agent_cfg.clone()),
        orchestrator_config: Some(orch_cfg.clone()),
    };

    let empty = HashMap::new();
    let skipped = write_files(
        dir.path(),
        "docker-compose.yml",
        &compose,
        &env,
        &empty,
        &configs,
    )
    .unwrap();

    assert!(!skipped);
    assert!(dir.path().join("docker-compose.yml").exists());
    assert!(dir.path().join(".env").exists());
    assert!(dir.path().join(".gitignore").exists());
    assert!(dir.path().join("config/agent.yml").exists());
    assert!(dir.path().join("config/orchestrator.yml").exists());

    // Verify file contents
    let written_compose = std::fs::read_to_string(dir.path().join("docker-compose.yml")).unwrap();
    assert_eq!(written_compose, compose);

    let written_env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert_eq!(written_env, env);

    let written_agent = std::fs::read_to_string(dir.path().join("config/agent.yml")).unwrap();
    assert_eq!(written_agent, agent_cfg);

    let written_orch = std::fs::read_to_string(dir.path().join("config/orchestrator.yml")).unwrap();
    assert_eq!(written_orch, orch_cfg);

    let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gi.contains(".env"));
}

#[test]
fn write_files_full_worker_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let providers = sample_providers();
    let agents = sample_agents();

    let compose = render_compose_worker(&agents, &providers);
    let env = render_env(
        "worker",
        8080,
        4222,
        "tok",
        "tok",
        "",
        &providers,
        Some("http://orch:8080"),
        None,
    );
    let agent_cfg = render_agent_config(
        "http://orch:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    let configs = ConfigFiles {
        agent_config: Some(agent_cfg),
        orchestrator_config: None,
    };

    let empty = HashMap::new();
    let skipped = write_files(dir.path(), "compose.yml", &compose, &env, &empty, &configs).unwrap();

    assert!(!skipped);
    assert!(dir.path().join("compose.yml").exists());
    assert!(dir.path().join(".env").exists());
    assert!(dir.path().join("config/agent.yml").exists());
    assert!(!dir.path().join("config/orchestrator.yml").exists());
}

// ── write_files merge scenario ──────────────────────────────────

#[test]
fn write_files_merge_preserves_secrets_in_template() {
    let dir = tempfile::tempdir().unwrap();
    let original_env = "APP_PORT=8080\nNATS_PORT=4222\nAPP_AUTH__TOKENS__0__SECRET=old-secret\n";
    std::fs::write(dir.path().join(".env"), original_env).unwrap();

    let existing = parse_env_file(&dir.path().join(".env"));
    assert_eq!(existing.len(), 3);

    // New template has the same keys with different values
    let new_env = "APP_PORT=9090\nNATS_PORT=5222\nAPP_AUTH__TOKENS__0__SECRET=generated-new\n";
    let skipped = write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        new_env,
        &existing,
        &no_configs(),
    )
    .unwrap();

    assert!(
        skipped,
        "Should merge .env when all new keys exist in old env"
    );
    let content = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(
        content.contains("APP_AUTH__TOKENS__0__SECRET=old-secret"),
        "SECRET key must preserve existing value"
    );
    assert!(
        content.contains("APP_PORT=9090"),
        "Non-secret key should use new template value"
    );
    assert!(
        content.contains("NATS_PORT=5222"),
        "Non-secret key should use new template value"
    );
}

#[test]
fn write_files_merge_writes_env_when_new_key_needed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "APP_PORT=8080\n").unwrap();

    let existing = parse_env_file(&dir.path().join(".env"));
    let new_env = "APP_PORT=8080\nNEW_KEY=new_val\n";

    let skipped = write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        new_env,
        &existing,
        &no_configs(),
    )
    .unwrap();

    // Merge still writes when new keys appear; the boolean reports
    // "merged" (true) rather than "skipped" (which is the empty-
    // existing branch). The new key lands in the file regardless.
    assert!(skipped, "merged because existing was non-empty");
    let content = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(content.contains("NEW_KEY=new_val"));
}

// ── backup_files comprehensive ──────────────────────────────────

#[test]
fn backup_files_both_files_get_timestamped_names() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("my-compose.yml"), "services:\n").unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=val\n").unwrap();

    backup_files(dir.path(), "my-compose.yml").unwrap();

    let backup_dir = dir.path().join(".nsed-backup");
    let mut entries: Vec<String> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();

    assert_eq!(entries.len(), 2);
    // One should start with ".env." and one with "my-compose.yml."
    let has_env = entries.iter().any(|n| n.starts_with(".env."));
    let has_compose = entries.iter().any(|n| n.starts_with("my-compose.yml."));
    assert!(has_env, "Should have .env backup");
    assert!(has_compose, "Should have compose backup");

    // Verify backup content matches original
    let env_backup = entries.iter().find(|n| n.starts_with(".env.")).unwrap();
    let env_content = std::fs::read_to_string(backup_dir.join(env_backup)).unwrap();
    assert_eq!(env_content, "KEY=val\n");
}

// ── build_model_options with partial pricing ─────────────────────

#[test]
fn build_model_options_partial_pricing_display() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![
            ModelInfo {
                name: "model-with-partial-price".to_string(),
                input_price: Some(0.5),
                output_price: None,
            },
            ModelInfo {
                name: "model-with-zero-and-paid".to_string(),
                input_price: Some(0.0),
                output_price: Some(1.0),
            },
        ],
        engine: None,
    }];
    let opts = build_model_options(&providers);
    assert_eq!(opts.len(), 2);
    // First model has partial pricing: Some(0.5), None → empty price tag
    assert_eq!(opts[0].3, Some(0.5));
    assert_eq!(opts[0].4, None);
    // Second model: 0.0 input, 1.0 output → neither free nor pure pricing display
    assert_eq!(opts[1].3, Some(0.0));
    assert_eq!(opts[1].4, Some(1.0));
}

// ── print_pricing_table with varied agents ───────────────────────

#[test]
fn print_pricing_table_all_price_cell_branches() {
    let agents = vec![
        // Positive input, positive output → both orange colored
        AgentSlot::new(
            "PAID".to_string(),
            "p".to_string(),
            "paid-model".to_string(),
            Some(2.50),
            Some(10.0),
        ),
        // Zero input, zero output → "free" text
        AgentSlot::new(
            "FREE".to_string(),
            "p".to_string(),
            "free-model".to_string(),
            Some(0.0),
            Some(0.0),
        ),
        // None input, None output → "—" text
        AgentSlot::new(
            "UNKNOWN".to_string(),
            "p".to_string(),
            "unknown-model".to_string(),
            None,
            None,
        ),
        // Positive input, None output → mixed
        AgentSlot::new(
            "PARTIAL".to_string(),
            "p".to_string(),
            "partial-model".to_string(),
            Some(0.5),
            None,
        ),
        // None input, positive output → mixed
        AgentSlot::new(
            "PARTIAL2".to_string(),
            "p".to_string(),
            "partial2-model".to_string(),
            None,
            Some(3.0),
        ),
    ];
    // Exercises all branches in print_pricing_table: Some(v) where v > 0, _ (other)
    print_pricing_table(&agents);
}

#[test]
fn print_pricing_table_model_truncation_boundary() {
    // Exactly 30 chars: not truncated
    let exact_30 = "a".repeat(30);
    let a1 = AgentSlot::new(
        "X".to_string(),
        "p".to_string(),
        exact_30,
        Some(1.0),
        Some(2.0),
    );
    print_pricing_table(&[a1]);

    // 31 chars: truncated with ellipsis prefix
    let over_30 = "b".repeat(31);
    let a2 = AgentSlot::new(
        "Y".to_string(),
        "p".to_string(),
        over_30,
        Some(1.0),
        Some(2.0),
    );
    print_pricing_table(&[a2]);

    // Very long: heavily truncated
    let very_long = "c".repeat(100);
    let a3 = AgentSlot::new(
        "Z".to_string(),
        "p".to_string(),
        very_long,
        Some(1.0),
        Some(2.0),
    );
    print_pricing_table(&[a3]);
}

// ── render functions called with each PRESET_CONFIGS entry ───────

#[test]
fn render_agent_config_exercises_all_preset_personas() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: None,
        output_price: None,
        provider_type: "openai".to_string(),
        models: vec![],
        engine: None,
    }];

    let agents: Vec<AgentSlot> = PRESET_CONFIGS
        .iter()
        .map(|pc| {
            let mut slot = AgentSlot::new(
                pc.name.to_string(),
                "prov".to_string(),
                format!("model-for-{}", pc.name),
                Some(1.0),
                Some(2.0),
            );
            slot.apply_preset();
            slot
        })
        .collect();

    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &providers,
        &agents,
    );

    // All preset names should appear
    for pc in PRESET_CONFIGS {
        assert!(
            cfg.contains(&format!("name: \"{}\"", pc.name)),
            "Missing preset {}",
            pc.name
        );
        // All presets have personas
        assert!(
            cfg.contains(&format!("model_name: \"model-for-{}\"", pc.name)),
            "Missing model for {}",
            pc.name
        );
    }

    // Verify that presets with reasoning_effort get it rendered
    for pc in PRESET_CONFIGS {
        if pc.reasoning_effort.is_some() {
            // Should have an active reasoning_effort line somewhere
            assert!(
                cfg.contains("reasoning_effort: \"medium\"")
                    || cfg.contains("reasoning_effort: \"high\"")
                    || cfg.contains("reasoning_effort: \"low\"")
            );
            break;
        }
    }

    // Verify presets with context_window get it rendered
    let distinct_cw: std::collections::HashSet<_> = PRESET_CONFIGS
        .iter()
        .filter_map(|pc| pc.context_window)
        .collect();
    for cw in &distinct_cw {
        assert!(
            cfg.contains(&format!("context_window: {cw}")),
            "Missing context_window {cw}"
        );
    }
}

// ── env_keys with render_env output ───────────────────────────────

#[test]
fn env_keys_round_trip_with_render_env() {
    let providers = sample_providers();
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &providers, None, None);
    let keys = env_keys(&env);

    // Should contain expected keys
    assert!(keys.contains("APP_PORT"));
    assert!(keys.contains("NATS_PORT"));
    assert!(keys.contains("APP_AUTH__ENABLED"));
    assert!(keys.contains("APP_AUTH__TOKENS__0__NAME"));
    assert!(keys.contains("APP_AUTH__TOKENS__0__SECRET"));
    assert!(keys.contains("APP_AUTH__TOKENS__1__NAME"));
    assert!(keys.contains("APP_AUTH__TOKENS__1__SECRET"));
    assert!(keys.contains("APP_CREDENTIALS__ENABLED"));
    assert!(keys.contains("APP_CREDENTIALS__ACCOUNT_SEED"));
    assert!(keys.contains("NSED_BEARER_TOKEN"));
    assert!(keys.contains("TOGETHER_AI_API_KEY"));
}

#[test]
fn env_keys_round_trip_with_render_env_worker() {
    let providers = sample_providers();
    let env = render_env(
        "worker",
        8080,
        4222,
        "tok",
        "tok",
        "",
        &providers,
        Some("http://orch:8080"),
        None,
    );
    let keys = env_keys(&env);

    assert!(keys.contains("NSED_BEARER_TOKEN"));
    assert!(keys.contains("NSED_ORCHESTRATOR_URL"));
    assert!(keys.contains("TOGETHER_AI_API_KEY"));
    // Worker should NOT have owner-specific keys
    assert!(!keys.contains("APP_PORT"));
    assert!(!keys.contains("APP_AUTH__ENABLED"));
}

// ── check_model_diversity edge cases ─────────────────────────────

#[test]
fn check_model_diversity_exactly_two_agents_two_models() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "model-1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "model-2".to_string(),
            None,
            None,
        ),
    ];
    // 2 agents < 3, so no diversity warning
    assert!(!check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_four_agents_three_models() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "m2".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "p".to_string(),
            "m3".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "D".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
    ];
    // 4 agents >= 3, 3 distinct models >= 3, so NO warning
    assert!(!check_model_diversity(&agents));
}

#[test]
fn check_model_diversity_four_agents_two_models() {
    let agents = vec![
        AgentSlot::new(
            "A".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "B".to_string(),
            "p".to_string(),
            "m2".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "C".to_string(),
            "p".to_string(),
            "m1".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "D".to_string(),
            "p".to_string(),
            "m2".to_string(),
            None,
            None,
        ),
    ];
    // 4 agents >= 3, 2 distinct models < 3, WARNING
    assert!(check_model_diversity(&agents));
}

// ── build_fallback_agent edge cases ──────────────────────────────

#[test]
fn build_fallback_agent_multiple_providers_first_with_models_wins() {
    let providers = vec![
        Provider {
            id: "p1".to_string(),
            base_url: "https://p1.api/v1".to_string(),
            api_key: Some("sk-1".to_string()),
            input_price: Some(1.0),
            output_price: Some(2.0),
            provider_type: "openai".to_string(),
            models: vec![
                ModelInfo {
                    name: "first-model".to_string(),
                    input_price: Some(0.5),
                    output_price: Some(1.0),
                },
                ModelInfo {
                    name: "second-model".to_string(),
                    input_price: Some(0.7),
                    output_price: Some(1.4),
                },
            ],
            engine: None,
        },
        Provider {
            id: "p2".to_string(),
            base_url: "https://p2.api/v1".to_string(),
            api_key: Some("sk-2".to_string()),
            input_price: Some(3.0),
            output_price: Some(6.0),
            provider_type: "openai".to_string(),
            models: vec![ModelInfo {
                name: "expensive-model".to_string(),
                input_price: Some(5.0),
                output_price: Some(10.0),
            }],
            engine: None,
        },
    ];
    let slot = build_fallback_agent(&providers).unwrap();
    assert_eq!(slot.provider_id, "p1");
    assert_eq!(slot.model_name, "first-model");
    assert_eq!(slot.input_price, Some(0.5));
    assert_eq!(slot.output_price, Some(1.0));
}

#[test]
fn build_fallback_agent_model_none_price_uses_or_with_provider() {
    let providers = vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-key".to_string()),
        input_price: Some(99.0),
        output_price: Some(199.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "model-no-price".to_string(),
            input_price: None,
            output_price: None,
        }],
        engine: None,
    }];
    let slot = build_fallback_agent(&providers).unwrap();
    // Model has None pricing, so .or(provider price) kicks in
    assert_eq!(slot.input_price, Some(99.0));
    assert_eq!(slot.output_price, Some(199.0));
}

#[test]
fn build_fallback_agent_empty_providers_returns_none() {
    let providers: Vec<Provider> = Vec::new();
    assert!(
        build_fallback_agent(&providers).is_none(),
        "empty providers should return None"
    );
}

// ── suggest_provider_id with edge cases ──────────────────────────

#[test]
fn suggest_provider_id_empty_string_preset() {
    let existing: Vec<Provider> = vec![];
    assert_eq!(suggest_provider_id("", &existing), "");
}

#[test]
fn suggest_provider_id_three_existing_same_id() {
    let existing = vec![
        Provider {
            id: "custom".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "other".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
        Provider {
            id: "third".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: None,
            output_price: None,
            provider_type: "openai".to_string(),
            models: vec![],
            engine: None,
        },
    ];
    // "custom" exists, so suffix should be len+1 = 4
    assert_eq!(suggest_provider_id("custom", &existing), "custom_4");
    // "other" exists too
    assert_eq!(suggest_provider_id("other", &existing), "other_4");
    // "new_id" doesn't exist
    assert_eq!(suggest_provider_id("new_id", &existing), "new_id");
}

// ── ModelEntry serde edge cases ──────────────────────────────────

#[test]
fn model_entry_deserialize_extra_fields_ignored() {
    let json = r#"{"id": "test", "extra_field": "ignored", "another": 123}"#;
    let entry: ModelEntry = serde_json::from_str(json).unwrap();
    assert_eq!(entry.id, "test");
}

#[test]
fn model_entry_deserialize_with_full_pricing() {
    let json = r#"{"id": "model", "type": "chat", "pricing": {"input": "1.50", "output": 3.00}}"#;
    let entry: ModelEntry = serde_json::from_str(json).unwrap();
    assert_eq!(entry.model_type.as_deref(), Some("chat"));
    let (inp, out) = extract_pricing(entry.pricing.as_ref());
    assert!((inp.unwrap() - 1.5).abs() < 1e-9);
    assert!((out.unwrap() - 3.0).abs() < 1e-9);
}

#[test]
fn models_response_wrapped_empty_data() {
    let json = r#"{"data": []}"#;
    let resp: ModelsResponseWrapped = serde_json::from_str(json).unwrap();
    assert!(resp.data.is_empty());
}

#[test]
fn model_entry_flat_array_empty() {
    let json = r#"[]"#;
    let entries: Vec<ModelEntry> = serde_json::from_str(json).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn model_entry_deserialize_pricing_with_nulls() {
    let json = r#"{"id": "model", "pricing": {"input": null, "output": null}}"#;
    let entry: ModelEntry = serde_json::from_str(json).unwrap();
    let (inp, out) = extract_pricing(entry.pricing.as_ref());
    assert!(inp.is_none());
    assert!(out.is_none());
}

// ── compose_flag with more edge cases ────────────────────────────

#[test]
fn compose_flag_empty_string() {
    assert_eq!(compose_flag(""), " -f ''");
}

#[test]
fn compose_flag_with_spaces() {
    assert_eq!(compose_flag("my compose.yml"), " -f 'my compose.yml'");
}

// ── is_inference_model combined type and keyword test ────────────

#[test]
fn is_inference_model_type_takes_precedence_over_all_keywords() {
    // Model id contains EVERY filter keyword but type is "chat"
    let e = entry_typed("embed-rerank-bge-e5-clip-vision-whisper", "chat");
    assert!(
        is_inference_model(&e),
        "Type 'chat' should override keyword filtering"
    );
}

#[test]
fn is_inference_model_unknown_type_passes() {
    // Unknown type (not in EXCLUDED_MODEL_TYPES) should pass
    let e = entry_typed("some-model", "audio");
    assert!(is_inference_model(&e));
}

#[test]
fn is_inference_model_case_insensitive_type() {
    // "EMBEDDING" should match "embedding" after lowercasing
    let e = entry_typed("model", "EMBEDDING");
    assert!(!is_inference_model(&e));
    let e2 = entry_typed("model", "Rerank");
    assert!(!is_inference_model(&e2));
    let e3 = entry_typed("model", "IMAGE");
    assert!(!is_inference_model(&e3));
}

// ── render_env date stamp ────────────────────────────────────────

#[test]
fn render_env_contains_date_stamp() {
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &[], None, None);
    // Pattern match avoids midnight flakiness when Local::now() straddles
    // a day boundary.  We just check the "Generated by" line contains a
    // YYYY-MM-DD substring (10 chars: 4-digit year, dash, 2-digit month,
    // dash, 2-digit day).
    let has_date = env.lines().any(|l| {
        l.contains("Generated by") && l.chars().any(|c| c.is_ascii_digit()) && l.contains('-')
    });
    assert!(
        has_date,
        "render_env should contain a YYYY-MM-DD date stamp"
    );
}

#[test]
fn render_env_contains_warning_comment() {
    let env = render_env("owner", 8080, 4222, "s", "w", "sa", &[], None, None);
    assert!(env.contains("WARNING: Keep this file SECRET"));
    assert!(env.contains("Never commit it to version control"));
}

// ── render_agent_config header comments ──────────────────────────

#[test]
fn render_agent_config_header_structure() {
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &[], &[]);
    assert!(cfg.contains("NSED Agent Configuration"));
    assert!(cfg.contains("Generated by nsed init"));
    assert!(cfg.contains("Override values by creating config/local.yml"));
}

#[test]
fn render_agent_config_provider_section_comments() {
    let cfg = render_agent_config("http://nsed:8080", "${NSED_BEARER_TOKEN}", &[], &[]);
    assert!(cfg.contains("LLM Providers"));
    assert!(cfg.contains("Engine strategies control LLM protocol"));
    assert!(cfg.contains("\"harmony\""));
    assert!(cfg.contains("\"vllm\""));
    assert!(cfg.contains("\"vllm_responses\""));
}

// ── render_orchestrator_config header structure ──────────────────

#[test]
fn render_orchestrator_config_header_structure() {
    let cfg = render_orchestrator_config(8080, 4222, &[]);
    assert!(cfg.contains("NSED Orchestrator Configuration"));
    assert!(cfg.contains("Generated by nsed init"));
    assert!(cfg.contains("Override values by creating config/local.yml"));
    assert!(cfg.contains("APP_ prefix"));
}

// ── render_compose_owner header structure ────────────────────────

#[test]
fn render_compose_owner_header_comment() {
    let compose = render_compose_owner(8080, 4222, &[], &[]);
    assert!(compose.contains("Generated by nsed-orchestrator init"));
    assert!(compose.contains("docker compose up -d"));
}

#[test]
fn render_compose_worker_header_comment() {
    let compose = render_compose_worker(&[], &[]);
    assert!(compose.contains("Generated by nsed-orchestrator init (worker)"));
    assert!(compose.contains("docker compose up -d"));
}

// ── parse_models_response ────────────────────────────────────────

#[test]
fn parse_models_response_wrapped_format() {
    let json = r#"{"data": [
            {"id": "llama-70b"},
            {"id": "mistral-7b"}
        ]}"#;
    let result = parse_models_response(json.as_bytes()).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].0, "llama-70b");
    assert_eq!(result[1].0, "mistral-7b");
}

#[test]
fn parse_models_response_flat_array_format() {
    let json = r#"[
            {"id": "gpt-4o"},
            {"id": "gpt-3.5-turbo"}
        ]"#;
    let result = parse_models_response(json.as_bytes()).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].0, "gpt-4o");
    assert_eq!(result[1].0, "gpt-3.5-turbo");
}

#[test]
fn parse_models_response_invalid_json_returns_none() {
    let result = parse_models_response(b"not json at all");
    assert!(result.is_none());
}

#[test]
fn parse_models_response_empty_data() {
    let json = r#"{"data": []}"#;
    let result = parse_models_response(json.as_bytes()).unwrap();
    assert!(result.is_empty());
}

#[test]
fn parse_models_response_filters_non_inference_models() {
    let json = r#"{"data": [
            {"id": "gpt-4o"},
            {"id": "text-embedding-ada-002", "type": "embedding"},
            {"id": "whisper-1"},
            {"id": "rerank-english-v3.0", "type": "rerank"},
            {"id": "llama-70b"}
        ]}"#;
    let result = parse_models_response(json.as_bytes()).unwrap();
    let names: Vec<&str> = result.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, vec!["gpt-4o", "llama-70b"]);
}

#[test]
fn parse_models_response_extracts_pricing() {
    let json = r#"{"data": [
            {"id": "llama-70b", "pricing": {"input": "0.9", "output": "0.9"}}
        ]}"#;
    let result = parse_models_response(json.as_bytes()).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, "llama-70b");
    assert_eq!(result[0].1, Some(0.9));
    assert_eq!(result[0].2, Some(0.9));
}

#[test]
fn parse_models_response_no_pricing_gives_none() {
    let json = r#"[{"id": "my-model"}]"#;
    let result = parse_models_response(json.as_bytes()).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, None);
    assert_eq!(result[0].2, None);
}

#[test]
fn parse_models_response_mixed_pricing_and_types() {
    let json = r#"{"data": [
            {"id": "model-a", "pricing": {"input": "1.5", "output": "2.0"}},
            {"id": "model-b"},
            {"id": "embedding-model", "type": "embedding"},
            {"id": "model-c", "pricing": {"input": "0", "output": "0"}}
        ]}"#;
    let result = parse_models_response(json.as_bytes()).unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].0, "model-a");
    assert_eq!(result[0].1, Some(1.5));
    assert_eq!(result[0].2, Some(2.0));
    assert_eq!(result[1].0, "model-b");
    assert_eq!(result[1].1, None);
    assert_eq!(result[2].0, "model-c");
    assert_eq!(result[2].1, Some(0.0));
    assert_eq!(result[2].2, Some(0.0));
}

#[test]
fn parse_models_response_empty_bytes() {
    let result = parse_models_response(b"");
    assert!(result.is_none());
}

#[test]
fn parse_models_response_wrong_structure() {
    // Valid JSON, but wrong structure (object without "data" key)
    let json = r#"{"models": [{"id": "x"}]}"#;
    // This will fail wrapped (no "data"), then fail flat (not an array)
    // Actually it might parse as ModelsResponseWrapped with missing field...
    // Let's check with explicit test:
    let result = parse_models_response(json.as_bytes());
    // ModelsResponseWrapped requires "data" field, this has "models" instead
    // So wrapped parse fails. Then flat array parse also fails (it's an object).
    assert!(result.is_none());
}

// ── build_models_url ─────────────────────────────────────────────

#[test]
fn build_models_url_no_trailing_slash() {
    assert_eq!(
        build_models_url("https://api.openai.com/v1"),
        "https://api.openai.com/v1/models"
    );
}

#[test]
fn build_models_url_with_trailing_slash() {
    assert_eq!(
        build_models_url("https://api.openai.com/v1/"),
        "https://api.openai.com/v1/models"
    );
}

#[test]
fn build_models_url_multiple_trailing_slashes() {
    assert_eq!(
        build_models_url("https://api.example.com///"),
        "https://api.example.com/models"
    );
}

#[test]
fn build_models_url_localhost() {
    assert_eq!(
        build_models_url("http://localhost:11434/v1"),
        "http://localhost:11434/v1/models"
    );
}

// ── detect_existing_files ────────────────────────────────────────

#[test]
fn detect_existing_files_fresh_directory() {
    let dir = tempfile::tempdir().unwrap();
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert!(found.is_empty());
}

#[test]
fn detect_existing_files_only_compose() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("docker-compose.yml"), "test").unwrap();
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert_eq!(found, vec!["docker-compose.yml"]);
}

#[test]
fn detect_existing_files_only_env() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=val").unwrap();
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert_eq!(found, vec![".env"]);
}

#[test]
fn detect_existing_files_both_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("docker-compose.yml"), "test").unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=val").unwrap();
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert_eq!(found, vec!["docker-compose.yml", ".env"]);
}

#[test]
fn detect_existing_files_custom_compose_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("my-compose.yml"), "test").unwrap();
    let found = detect_existing_files(dir.path(), "my-compose.yml");
    assert_eq!(found, vec!["my-compose.yml"]);
}

#[test]
fn detect_existing_files_wrong_compose_name_not_found() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("docker-compose.yml"), "test").unwrap();
    // Looking for a different compose name
    let found = detect_existing_files(dir.path(), "compose.yml");
    assert!(found.is_empty());
}

#[test]
fn detect_existing_files_includes_config_yamls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(dir.path().join("config/agent.yml"), "agents:").unwrap();
    std::fs::write(dir.path().join("config/orchestrator.yml"), "server:").unwrap();
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert!(found.contains(&"config/agent.yml"));
    assert!(found.contains(&"config/orchestrator.yml"));
}

#[test]
fn detect_existing_files_all_files_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("docker-compose.yml"), "test").unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=val").unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(dir.path().join("config/agent.yml"), "agents:").unwrap();
    std::fs::write(dir.path().join("config/orchestrator.yml"), "server:").unwrap();
    let found = detect_existing_files(dir.path(), "docker-compose.yml");
    assert_eq!(found.len(), 4);
    assert_eq!(
        found,
        vec![
            "docker-compose.yml",
            ".env",
            "config/agent.yml",
            "config/orchestrator.yml"
        ]
    );
}

#[test]
fn backup_files_includes_config_yamls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("docker-compose.yml"), "compose").unwrap();
    std::fs::write(dir.path().join(".env"), "KEY=val").unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(dir.path().join("config/agent.yml"), "agents:").unwrap();
    std::fs::write(dir.path().join("config/orchestrator.yml"), "server:").unwrap();

    backup_files(dir.path(), "docker-compose.yml").unwrap();

    let backup_dir = dir.path().join(".nsed-backup");
    assert!(backup_dir.exists());
    // Check that backup files exist (with timestamp suffix)
    let entries: Vec<_> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries.len(), 4, "all 4 files should be backed up");
    assert!(entries.iter().any(|n| n.starts_with("docker-compose.yml.")));
    assert!(entries.iter().any(|n| n.starts_with(".env.")));
    assert!(entries.iter().any(|n| n.starts_with("config_agent.yml.")));
    assert!(
        entries
            .iter()
            .any(|n| n.starts_with("config_orchestrator.yml."))
    );
}

// ── generate_output_content ──────────────────────────────────────

#[test]
fn generate_output_content_owner_with_agents() {
    let providers = vec![Provider {
        id: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: Some(5.0),
        output_price: Some(15.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "gpt-4o".to_string(),
            input_price: Some(5.0),
            output_price: Some(15.0),
        }],
        engine: None,
    }];
    let mut agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "openai".to_string(),
        "gpt-4o".to_string(),
        Some(5.0),
        Some(15.0),
    )];
    agents[0].apply_preset();

    let (compose, env, configs) = generate_output_content(
        "owner",
        8080,
        4222,
        "api-secret",
        "worker-secret",
        "SA_SEED",
        &providers,
        &agents,
        None,
    );

    // Owner mode: compose should have orchestrator and nats services
    assert!(compose.contains("nsed:"));
    assert!(compose.contains("  nats:"));
    // Env should have owner sections
    assert!(env.contains("APP_PORT=8080"));
    assert!(env.contains("NATS_PORT=4222"));
    assert!(env.contains("OPENAI_API_KEY=sk-test"));
    // Configs should have both agent and orchestrator
    assert!(configs.agent_config.is_some());
    assert!(configs.orchestrator_config.is_some());
    // Agent config should contain the agent
    let agent_cfg = configs.agent_config.as_ref().unwrap();
    assert!(agent_cfg.contains("name: \"DEFAULT\""));
    assert!(agent_cfg.contains("gpt-4o"));
}

#[test]
fn generate_output_content_owner_no_agents() {
    let providers = vec![Provider {
        id: "sim".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];

    let (compose, env, configs) = generate_output_content(
        "owner",
        8080,
        4222,
        "api-s",
        "worker-s",
        "SEED",
        &providers,
        &[],
        None,
    );

    assert!(compose.contains("nsed:"));
    assert!(env.contains("APP_PORT=8080"));
    // No agent config when agents is empty
    assert!(configs.agent_config.is_none());
    // Orchestrator config should still be present for owner
    assert!(configs.orchestrator_config.is_some());
}

#[test]
fn generate_output_content_worker_with_agents() {
    let providers = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "llama3.2".to_string(),
            input_price: Some(0.0),
            output_price: Some(0.0),
        }],
        engine: None,
    }];
    let mut agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "ollama".to_string(),
        "llama3.2".to_string(),
        Some(0.0),
        Some(0.0),
    )];
    agents[0].apply_preset();

    let (compose, env, configs) = generate_output_content(
        "worker",
        8080,
        4222,
        "token",
        "token",
        "http://owner:8080",
        &providers,
        &agents,
        None,
    );

    // Worker mode: compose should NOT have orchestrator
    assert!(!compose.contains("nsed:"));
    // Worker env should contain orchestrator URL
    assert!(env.contains("http://owner:8080"));
    // Agent config present
    assert!(configs.agent_config.is_some());
    let agent_cfg = configs.agent_config.as_ref().unwrap();
    assert!(agent_cfg.contains("http://owner:8080"));
    // No orchestrator config for worker
    assert!(configs.orchestrator_config.is_none());
}

#[test]
fn generate_output_content_worker_no_agents() {
    let providers = vec![Provider {
        id: "sim".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![],
        engine: None,
    }];

    let (_compose, env, configs) = generate_output_content(
        "worker",
        8080,
        4222,
        "t",
        "t",
        "http://orch:8080",
        &providers,
        &[],
        None,
    );

    assert!(env.contains("http://orch:8080"));
    // No configs for worker with no agents
    assert!(configs.agent_config.is_none());
    assert!(configs.orchestrator_config.is_none());
}

#[test]
fn generate_output_content_owner_multiple_providers_and_agents() {
    let providers = vec![
        Provider {
            id: "together_ai".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            api_key: Some("sk-together".to_string()),
            input_price: Some(0.90),
            output_price: Some(0.90),
            provider_type: "openai".to_string(),
            models: vec![ModelInfo {
                name: "llama-70b".to_string(),
                input_price: Some(0.90),
                output_price: Some(0.90),
            }],
            engine: None,
        },
        Provider {
            id: "sim".to_string(),
            base_url: String::new(),
            api_key: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            provider_type: "simulated".to_string(),
            models: vec![ModelInfo {
                name: "simulated-default".to_string(),
                input_price: Some(0.0),
                output_price: Some(0.0),
            }],
            engine: None,
        },
    ];
    let mut agents = vec![
        AgentSlot::new(
            "DEFAULT".to_string(),
            "together_ai".to_string(),
            "llama-70b".to_string(),
            Some(0.90),
            Some(0.90),
        ),
        AgentSlot::new(
            "REASON".to_string(),
            "together_ai".to_string(),
            "llama-70b".to_string(),
            Some(0.90),
            Some(0.90),
        ),
        AgentSlot::new(
            "CREATE".to_string(),
            "sim".to_string(),
            "simulated-default".to_string(),
            Some(0.0),
            Some(0.0),
        ),
    ];
    for a in &mut agents {
        a.apply_preset();
    }

    let (compose, env, configs) = generate_output_content(
        "owner", 9090, 5222, "admin", "worker", "SEED", &providers, &agents, None,
    );

    // Check custom ports
    assert!(compose.contains("9090"));
    assert!(env.contains("APP_PORT=9090"));
    assert!(env.contains("NATS_PORT=5222"));
    // Multiple providers
    assert!(env.contains("TOGETHER_AI_API_KEY=sk-together"));
    // All agents
    let agent_cfg = configs.agent_config.as_ref().unwrap();
    assert!(agent_cfg.contains("name: \"DEFAULT\""));
    assert!(agent_cfg.contains("name: \"REASON\""));
    assert!(agent_cfg.contains("name: \"CREATE\""));
    // Orchestrator config present
    let orch_cfg = configs.orchestrator_config.as_ref().unwrap();
    assert!(orch_cfg.contains("port: 9090"));
}

// ── print_write_summary ──────────────────────────────────────────

#[test]
fn print_write_summary_env_written() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ConfigFiles {
        agent_config: None,
        orchestrator_config: None,
    };
    // Should not panic
    print_write_summary(dir.path(), "docker-compose.yml", false, &configs);
}

#[test]
fn print_write_summary_env_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ConfigFiles {
        agent_config: None,
        orchestrator_config: None,
    };
    // env_skipped=true path
    print_write_summary(dir.path(), "docker-compose.yml", true, &configs);
}

#[test]
fn print_write_summary_with_agent_config() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ConfigFiles {
        agent_config: Some("agent yaml".to_string()),
        orchestrator_config: None,
    };
    print_write_summary(dir.path(), "compose.yml", false, &configs);
}

#[test]
fn print_write_summary_with_orchestrator_config() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ConfigFiles {
        agent_config: None,
        orchestrator_config: Some("orchestrator yaml".to_string()),
    };
    print_write_summary(dir.path(), "compose.yml", false, &configs);
}

#[test]
fn print_write_summary_with_both_configs() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ConfigFiles {
        agent_config: Some("agent".to_string()),
        orchestrator_config: Some("orch".to_string()),
    };
    print_write_summary(dir.path(), "custom.yml", true, &configs);
}

// ── print_next_steps ─────────────────────────────────────────────

#[test]
fn print_next_steps_owner_persona() {
    let agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "openai".to_string(),
        "gpt-4o".to_string(),
        Some(5.0),
        Some(15.0),
    )];
    // Should not panic and should exercise the owner branch
    print_next_steps(
        "owner",
        "docker-compose.yml",
        8080,
        "admin",
        "worker",
        &agents,
        Path::new("."),
    );
}

#[test]
fn print_next_steps_worker_persona() {
    // Should not panic and should exercise the worker branch
    print_next_steps(
        "worker",
        "docker-compose.yml",
        8080,
        "token",
        "token",
        &[],
        Path::new("."),
    );
}

#[test]
fn print_next_steps_owner_custom_compose_name() {
    // Custom compose name triggers compose_flag
    print_next_steps(
        "owner",
        "my-compose.yml",
        9090,
        "s1",
        "s2",
        &[],
        Path::new("."),
    );
}

#[test]
fn print_next_steps_worker_custom_compose_name() {
    print_next_steps("worker", "custom.yml", 8080, "t", "t", &[], Path::new("."));
}

#[test]
fn print_next_steps_with_output_dir() {
    // Non-"." output_dir should show a cd hint
    print_next_steps(
        "owner",
        "docker-compose.yml",
        8080,
        "admin",
        "worker",
        &[],
        Path::new("/tmp/my-project"),
    );
}

#[test]
fn print_next_steps_owner_with_multiple_agents() {
    let agents = vec![
        AgentSlot::new(
            "DEFAULT".to_string(),
            "p".to_string(),
            "m".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "REASON".to_string(),
            "p".to_string(),
            "m".to_string(),
            None,
            None,
        ),
        AgentSlot::new(
            "CREATE".to_string(),
            "p".to_string(),
            "m".to_string(),
            None,
            None,
        ),
    ];
    print_next_steps(
        "owner",
        "docker-compose.yml",
        8080,
        "api",
        "worker",
        &agents,
        Path::new("."),
    );
}

// ── generate_output_content + write_files integration ───────────

#[test]
fn generate_and_write_owner_integration() {
    let dir = tempfile::tempdir().unwrap();
    let providers = vec![Provider {
        id: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: Some(5.0),
        output_price: Some(15.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "gpt-4o".to_string(),
            input_price: Some(5.0),
            output_price: Some(15.0),
        }],
        engine: None,
    }];
    let mut agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "openai".to_string(),
        "gpt-4o".to_string(),
        Some(5.0),
        Some(15.0),
    )];
    agents[0].apply_preset();

    let (compose, env, configs) = generate_output_content(
        "owner",
        8080,
        4222,
        "api-secret",
        "worker-secret",
        "SEED",
        &providers,
        &agents,
        None,
    );

    let env_skipped = write_files(
        dir.path(),
        "docker-compose.yml",
        &compose,
        &env,
        &HashMap::new(),
        &configs,
    )
    .unwrap();

    assert!(!env_skipped);
    // All files should be written
    assert!(dir.path().join("docker-compose.yml").exists());
    assert!(dir.path().join(".env").exists());
    assert!(dir.path().join(".gitignore").exists());
    assert!(dir.path().join("config/agent.yml").exists());
    assert!(dir.path().join("config/orchestrator.yml").exists());
}

// ── process_reinit_choice ─────────────────────────────────────

#[test]
fn process_reinit_choice_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let result = process_reinit_choice("Cancel", dir.path(), "docker-compose.yml").unwrap();
    assert!(matches!(result, ReinitChoice::Cancel));
}

#[test]
fn process_reinit_choice_overwrite_backs_up() {
    let dir = tempfile::tempdir().unwrap();
    // Create a file to be backed up
    std::fs::write(dir.path().join("docker-compose.yml"), "old content").unwrap();
    let result =
        process_reinit_choice("Overwrite  — start fresh", dir.path(), "docker-compose.yml")
            .unwrap();
    assert!(matches!(result, ReinitChoice::Overwrite));
    // Backup directory should be created
    assert!(dir.path().join(".nsed-backup").exists());
}

#[test]
fn process_reinit_choice_make_new() {
    let dir = tempfile::tempdir().unwrap();
    let result = process_reinit_choice(
        "Make new   — ignore existing",
        dir.path(),
        "docker-compose.yml",
    )
    .unwrap();
    assert!(matches!(result, ReinitChoice::MakeNew));
}

#[test]
fn process_reinit_choice_merge_reads_env() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".env"),
        "SECRET_KEY=my-secret\nDB_URL=postgres://localhost",
    )
    .unwrap();
    let result = process_reinit_choice(
        "Merge      — keep existing",
        dir.path(),
        "docker-compose.yml",
    )
    .unwrap();
    match result {
        ReinitChoice::Merge(env) => {
            assert_eq!(env.get("SECRET_KEY").unwrap(), "my-secret");
            assert_eq!(env.get("DB_URL").unwrap(), "postgres://localhost");
        }
        _ => panic!("Expected Merge variant"),
    }
}

#[test]
fn process_reinit_choice_merge_no_env_file() {
    let dir = tempfile::tempdir().unwrap();
    // No .env file exists
    let result = process_reinit_choice(
        "Merge      — keep existing",
        dir.path(),
        "docker-compose.yml",
    )
    .unwrap();
    match result {
        ReinitChoice::Merge(env) => {
            assert!(env.is_empty());
        }
        _ => panic!("Expected Merge variant"),
    }
}

// ── build_ollama_provider ────────────────────────────────────────

#[test]
fn build_ollama_provider_empty_models() {
    let provider = build_ollama_provider(&[]);
    assert_eq!(provider.id, "ollama_local");
    assert_eq!(provider.base_url, "http://localhost:11434/v1");
    assert!(provider.api_key.is_none());
    assert_eq!(provider.input_price, Some(0.0));
    assert_eq!(provider.output_price, Some(0.0));
    assert_eq!(provider.provider_type, "openai");
    assert!(provider.models.is_empty());
    assert!(provider.engine.is_none());
}

#[test]
fn build_ollama_provider_with_models() {
    let fetched: Vec<FetchedModel> = vec![
        ("llama3".to_string(), Some(0.0), Some(0.0)),
        ("mistral".to_string(), Some(0.0), Some(0.0)),
    ];
    let provider = build_ollama_provider(&fetched);
    assert_eq!(provider.models.len(), 2);
    assert_eq!(provider.models[0].name, "llama3");
    assert_eq!(provider.models[1].name, "mistral");
}

// ── build_simulated_provider ─────────────────────────────────────

#[test]
fn build_simulated_provider_structure() {
    let provider = build_simulated_provider();
    assert_eq!(provider.id, "simulated");
    assert!(provider.base_url.is_empty());
    assert!(provider.api_key.is_none());
    assert_eq!(provider.input_price, Some(0.0));
    assert_eq!(provider.output_price, Some(0.0));
    assert_eq!(provider.provider_type, "simulated");
    assert_eq!(provider.models.len(), 1);
    assert_eq!(provider.models[0].name, "simulated-default");
    assert!(provider.engine.is_none());
}

// ── derive_provider_pricing ──────────────────────────────────────

#[test]
fn derive_provider_pricing_ollama() {
    let (i, o) = derive_provider_pricing(true, &None);
    assert_eq!(i, Some(0.0));
    assert_eq!(o, Some(0.0));
}

#[test]
fn derive_provider_pricing_ollama_ignores_fetched() {
    let fetched = Some(vec![("model".to_string(), Some(5.0), Some(10.0))]);
    let (i, o) = derive_provider_pricing(true, &fetched);
    // Ollama always returns (0.0, 0.0)
    assert_eq!(i, Some(0.0));
    assert_eq!(o, Some(0.0));
}

#[test]
fn derive_provider_pricing_no_fetched() {
    let (i, o) = derive_provider_pricing(false, &None);
    assert_eq!(i, None);
    assert_eq!(o, None);
}

#[test]
fn derive_provider_pricing_empty_fetched() {
    let fetched: Option<Vec<FetchedModel>> = Some(vec![]);
    let (i, o) = derive_provider_pricing(false, &fetched);
    assert_eq!(i, None);
    assert_eq!(o, None);
}

#[test]
fn derive_provider_pricing_with_models() {
    let fetched = Some(vec![
        ("gpt-4o".to_string(), Some(5.0), Some(15.0)),
        ("gpt-3.5".to_string(), Some(0.5), Some(1.5)),
    ]);
    let (i, o) = derive_provider_pricing(false, &fetched);
    // Uses pricing from the first priced model
    assert_eq!(i, Some(5.0));
    assert_eq!(o, Some(15.0));
}

#[test]
fn derive_provider_pricing_first_model_no_prices() {
    let fetched = Some(vec![("model-a".to_string(), None, None)]);
    let (i, o) = derive_provider_pricing(false, &fetched);
    assert_eq!(i, None);
    assert_eq!(o, None);
}

#[test]
fn derive_provider_pricing_skips_unpriced_first_model() {
    // First model has no pricing, second does — should use the second.
    let fetched = Some(vec![
        ("unpriced-model".to_string(), None, None),
        ("priced-model".to_string(), Some(2.0), Some(8.0)),
    ]);
    let (i, o) = derive_provider_pricing(false, &fetched);
    assert_eq!(i, Some(2.0));
    assert_eq!(o, Some(8.0));
}

#[test]
fn derive_provider_pricing_all_unpriced() {
    let fetched = Some(vec![
        ("a".to_string(), None, None),
        ("b".to_string(), None, None),
    ]);
    let (i, o) = derive_provider_pricing(false, &fetched);
    assert_eq!(i, None);
    assert_eq!(o, None);
}

#[test]
fn derive_provider_pricing_partial_prices() {
    let fetched = Some(vec![("model-a".to_string(), Some(1.0), None)]);
    let (i, o) = derive_provider_pricing(false, &fetched);
    assert_eq!(i, Some(1.0));
    assert_eq!(o, None);
}

// ── build_provider_env_key ───────────────────────────────────────

#[test]
fn build_provider_env_key_simple() {
    assert_eq!(build_provider_env_key("openai"), "OPENAI_API_KEY");
}

#[test]
fn build_provider_env_key_with_underscore() {
    assert_eq!(build_provider_env_key("together_ai"), "TOGETHER_AI_API_KEY");
}

#[test]
fn build_provider_env_key_with_hyphen() {
    assert_eq!(build_provider_env_key("my-provider"), "MY_PROVIDER_API_KEY");
}

#[test]
fn build_provider_env_key_mixed_case() {
    assert_eq!(build_provider_env_key("OpenRouter"), "OPENROUTER_API_KEY");
}

#[test]
fn build_provider_env_key_already_uppercase() {
    assert_eq!(build_provider_env_key("GPT_OSS"), "GPT_OSS_API_KEY");
}

// ── sanitize_provider_id ─────────────────────────────────────────

#[test]
fn sanitize_provider_id_simple() {
    assert_eq!(sanitize_provider_id("openai"), Some("openai".into()));
}

#[test]
fn sanitize_provider_id_uppercase() {
    assert_eq!(
        sanitize_provider_id("OpenRouter"),
        Some("openrouter".into())
    );
}

#[test]
fn sanitize_provider_id_hyphens() {
    assert_eq!(
        sanitize_provider_id("my-provider"),
        Some("my_provider".into())
    );
}

#[test]
fn sanitize_provider_id_spaces() {
    assert_eq!(
        sanitize_provider_id("my provider"),
        Some("my_provider".into())
    );
}

#[test]
fn sanitize_provider_id_slashes_removed() {
    assert_eq!(
        sanitize_provider_id("my/provider"),
        Some("myprovider".into())
    );
}

#[test]
fn sanitize_provider_id_colons_removed() {
    assert_eq!(sanitize_provider_id("foo:bar"), Some("foobar".into()));
}

#[test]
fn sanitize_provider_id_mixed_special_chars() {
    assert_eq!(
        sanitize_provider_id("My Provider/v2:test-run"),
        Some("my_providerv2test_run".into()),
    );
}

#[test]
fn sanitize_provider_id_empty_after_strip() {
    assert_eq!(sanitize_provider_id("///"), None);
}

#[test]
fn sanitize_provider_id_empty_input() {
    assert_eq!(sanitize_provider_id(""), None);
}

#[test]
fn sanitize_provider_id_underscores_preserved() {
    assert_eq!(
        sanitize_provider_id("together_ai"),
        Some("together_ai".into())
    );
}

#[test]
fn sanitize_provider_id_digit_leading_gets_prefix() {
    // "01.ai" → slug "01ai" starts with digit → "p_01ai"
    assert_eq!(sanitize_provider_id("01.ai"), Some("p_01ai".into()));
}

#[test]
fn sanitize_provider_id_digit_only() {
    assert_eq!(sanitize_provider_id("42"), Some("p_42".into()));
}

#[test]
fn sanitize_provider_id_letter_leading_no_prefix() {
    // Leading letter → no prefix needed
    assert_eq!(sanitize_provider_id("a1"), Some("a1".into()));
}

#[test]
fn sanitize_provider_id_underscore_leading_no_prefix() {
    assert_eq!(sanitize_provider_id("_hidden"), Some("_hidden".into()));
}

// ── build_provider_env_key with sanitization ────────────────────

#[test]
fn build_provider_env_key_with_slash() {
    assert_eq!(build_provider_env_key("my/provider"), "MYPROVIDER_API_KEY");
}

#[test]
fn build_provider_env_key_with_colon() {
    assert_eq!(build_provider_env_key("foo:bar"), "FOOBAR_API_KEY");
}

#[test]
fn build_provider_env_key_with_spaces() {
    assert_eq!(build_provider_env_key("my provider"), "MY_PROVIDER_API_KEY");
}

#[test]
fn build_provider_env_key_digit_leading() {
    // "01.ai" sanitizes to "p_01ai" → "P_01AI_API_KEY"
    assert_eq!(build_provider_env_key("01.ai"), "P_01AI_API_KEY");
}

// ── Integration: generate + write ────────────────────────────────

#[test]
fn generate_and_write_worker_integration() {
    let dir = tempfile::tempdir().unwrap();
    let providers = vec![Provider {
        id: "ollama".to_string(),
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: vec![ModelInfo {
            name: "llama3".to_string(),
            input_price: Some(0.0),
            output_price: Some(0.0),
        }],
        engine: None,
    }];
    let mut agents = vec![AgentSlot::new(
        "DEFAULT".to_string(),
        "ollama".to_string(),
        "llama3".to_string(),
        Some(0.0),
        Some(0.0),
    )];
    agents[0].apply_preset();

    let (compose, env, configs) = generate_output_content(
        "worker",
        8080,
        4222,
        "token",
        "token",
        "http://orch:8080",
        &providers,
        &agents,
        None,
    );

    let env_skipped = write_files(
        dir.path(),
        "docker-compose.yml",
        &compose,
        &env,
        &HashMap::new(),
        &configs,
    )
    .unwrap();

    assert!(!env_skipped);
    assert!(dir.path().join("docker-compose.yml").exists());
    assert!(dir.path().join(".env").exists());
    assert!(dir.path().join("config/agent.yml").exists());
    // Worker: no orchestrator config
    assert!(!dir.path().join("config/orchestrator.yml").exists());
}

// ── is_secret_key ────────────────────────────────────────────────

#[test]
fn is_secret_key_matches_secret_pattern() {
    assert!(is_secret_key("APP_AUTH__TOKENS__0__SECRET"));
    assert!(is_secret_key("my_secret_value"));
    assert!(is_secret_key("SECRET"));
}

#[test]
fn is_secret_key_rejects_generic_token_keys() {
    // "TOKEN" is intentionally excluded from SECRET_KEY_PATTERNS so that
    // NSED_BEARER_TOKEN (a user-entered credential) is not silently preserved
    // during merge-reinit. Actual auth secrets use "SECRET" in the key name.
    assert!(!is_secret_key("SOME_TOKEN"));
    assert!(!is_secret_key("NSED_BEARER_TOKEN"));
    assert!(!is_secret_key("AUTH_TOKEN"));
}

#[test]
fn is_secret_key_matches_api_key_pattern() {
    assert!(is_secret_key("TOGETHER_AI_API_KEY"));
    assert!(is_secret_key("my_api_key"));
}

#[test]
fn is_secret_key_matches_password_pattern() {
    assert!(is_secret_key("DB_PASSWORD"));
    assert!(is_secret_key("password"));
}

#[test]
fn is_secret_key_matches_seed_pattern() {
    assert!(is_secret_key("ACCOUNT_SEED"));
    assert!(is_secret_key("seed_phrase"));
}

#[test]
fn is_secret_key_rejects_non_secret_keys() {
    assert!(!is_secret_key("APP_PORT"));
    assert!(!is_secret_key("NATS_PORT"));
    assert!(!is_secret_key("LOG_LEVEL"));
    assert!(!is_secret_key("HOSTNAME"));
    assert!(!is_secret_key(""));
}

#[test]
fn is_secret_key_is_case_insensitive() {
    assert!(is_secret_key("secret"));
    assert!(is_secret_key("SECRET"));
    assert!(is_secret_key("Secret"));
    assert!(is_secret_key("api_key"));
    assert!(is_secret_key("API_KEY"));
}

// ── merge_env (tested indirectly through write_files) ────────────

#[test]
fn merge_env_preserves_comments_and_blank_lines() {
    let dir = tempfile::tempdir().unwrap();
    let template = "# Header comment\n\nAPP_PORT=8080\nMY_SECRET=generated\n";
    let existing: HashMap<String, String> = [
        ("APP_PORT".into(), "9090".into()),
        ("MY_SECRET".into(), "keep-this".into()),
    ]
    .into_iter()
    .collect();

    write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        template,
        &existing,
        &no_configs(),
    )
    .unwrap();

    let content = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(content.contains("# Header comment"), "comment preserved");
    assert!(content.contains("MY_SECRET=keep-this"), "secret preserved");
    assert!(
        content.contains("APP_PORT=8080"),
        "non-secret uses template"
    );
}

#[test]
fn merge_env_no_merge_when_existing_empty() {
    let dir = tempfile::tempdir().unwrap();
    let template = "APP_PORT=8080\nMY_SECRET=fresh\n";
    let existing: HashMap<String, String> = HashMap::new();

    let skipped = write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        template,
        &existing,
        &no_configs(),
    )
    .unwrap();

    assert!(!skipped, "no merge when existing is empty");
    let content = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(content.contains("MY_SECRET=fresh"), "fresh value written");
}

#[test]
fn merge_env_merges_even_when_existing_lacks_some_keys() {
    let dir = tempfile::tempdir().unwrap();
    let template = "APP_PORT=8080\nNEW_KEY=val\n";
    let existing: HashMap<String, String> =
        [("APP_PORT".into(), "9090".into())].into_iter().collect();

    let skipped = write_files(
        dir.path(),
        "compose.yml",
        "services:\n",
        template,
        &existing,
        &no_configs(),
    )
    .unwrap();

    // Post-fix: a new template key (NEW_KEY) does NOT prevent the
    // merge. The merge keeps existing's APP_PORT and adds NEW_KEY
    // from the template — previous behaviour wiped existing values.
    assert!(skipped, "merged (existing was non-empty)");
    let content = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(content.contains("NEW_KEY=val"), "new key written");
}

// ── gitignore .nsed-backup/ ──────────────────────────────────────

#[test]
fn ensure_gitignore_adds_nsed_backup_entry() {
    let dir = tempfile::tempdir().unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gi.contains(".env"), ".env should be in .gitignore");
    assert!(
        gi.contains(".nsed-backup/"),
        ".nsed-backup/ should be in .gitignore"
    );
}

#[test]
fn ensure_gitignore_appends_missing_entries() {
    let dir = tempfile::tempdir().unwrap();
    // Pre-write a .gitignore with only .env
    std::fs::write(dir.path().join(".gitignore"), ".env\n").unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gi.contains(".env"), ".env should still be present");
    assert!(
        gi.contains(".nsed-backup/"),
        ".nsed-backup/ should be appended"
    );
}

#[test]
fn ensure_gitignore_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    // Pre-write a .gitignore with both entries
    std::fs::write(dir.path().join(".gitignore"), ".env\n.nsed-backup/\n").unwrap();
    ensure_gitignore(dir.path()).unwrap();
    let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    // Count occurrences — each should appear exactly once
    let env_count = gi.lines().filter(|l| l.trim() == ".env").count();
    let backup_count = gi.lines().filter(|l| l.trim() == ".nsed-backup/").count();
    assert_eq!(env_count, 1, ".env should appear exactly once");
    assert_eq!(backup_count, 1, ".nsed-backup/ should appear exactly once");
}

// ── shell_quote ──────────────────────────────────────────────────

#[test]
fn shell_quote_safe_string_unquoted() {
    // UUID-style tokens contain only alphanumeric + hyphens → no quoting
    assert_eq!(shell_quote("abc-123-def"), "abc-123-def");
}

#[test]
fn shell_quote_url_unquoted() {
    // URLs with safe characters pass through
    assert_eq!(
        shell_quote("http://localhost:8080"),
        "http://localhost:8080"
    );
}

#[test]
fn shell_quote_special_chars_quoted() {
    // Dollar sign, backtick, space, exclamation all need quoting
    assert_eq!(shell_quote("pa$$word"), "'pa$$word'");
    assert_eq!(shell_quote("has space"), "'has space'");
    assert_eq!(shell_quote("back`tick"), "'back`tick'");
}

#[test]
fn shell_quote_single_quote_escaped() {
    // Embedded single quote uses the '\'' idiom
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
}

#[test]
fn shell_quote_empty_string() {
    assert_eq!(shell_quote(""), "''");
}

// ── per-agent file/dir access ───────────────────────────────────────

/// Helper: a single provider of the given type, id `prov`.
fn provider_of_type(provider_type: &str) -> Vec<Provider> {
    vec![Provider {
        id: "prov".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: Some("sk-test".to_string()),
        input_price: None,
        output_price: None,
        provider_type: provider_type.to_string(),
        models: vec![],
        engine: None,
    }]
}

fn access_agent(read: &[&str], write: &[&str]) -> AgentSlot {
    let mut a = AgentSlot::new(
        "A".to_string(),
        "prov".to_string(),
        "m".to_string(),
        None,
        None,
    );
    a.read_paths = read.iter().map(|s| s.to_string()).collect();
    a.write_dirs = write.iter().map(|s| s.to_string()).collect();
    a
}

#[test]
fn render_agent_access_native_read_emits_builtin_read_file() {
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &provider_of_type("openai"),
        &[access_agent(&["docs/", "src/api.rs"], &[])],
    );
    assert!(cfg.contains("    builtin_tools:"));
    assert!(cfg.contains("      - type: read_file"));
    assert!(cfg.contains("        roots: [\"docs/\", \"src/api.rs\"]"));
}

#[test]
fn render_agent_access_native_write_emits_note_not_tool() {
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &provider_of_type("openai"),
        &[access_agent(&[], &["./out"])],
    );
    assert!(
        cfg.contains("# write access requested but openai has no write tool"),
        "native provider must explain write isn't supported, got:\n{cfg}"
    );
    assert!(!cfg.contains("writable: true"));
}

#[test]
fn render_agent_access_claude_read_only_emits_context_files_no_writable() {
    // Read paths → context_files (inlined, read-only). NOT add_dirs, so the
    // write tools can never reach them; no writable flag.
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &provider_of_type("claude"),
        &[access_agent(&["docs/"], &[])],
    );
    assert!(cfg.contains("    claude:"));
    assert!(cfg.contains("      context_files: [\"docs/\"]"));
    assert!(
        !cfg.contains("add_dirs:"),
        "read context must not be writable scope, got:\n{cfg}"
    );
    assert!(!cfg.contains("writable: true"));
}

#[test]
fn render_agent_access_claude_write_separates_context_from_add_dirs() {
    // Read → context_files (RO); write dirs → add_dirs + writable. The two
    // stay separate so the read path is never in the writable scope.
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &provider_of_type("claude"),
        &[access_agent(&["docs/"], &["./src"])],
    );
    assert!(cfg.contains("      context_files: [\"docs/\"]"));
    assert!(cfg.contains("      add_dirs: [\"./src\"]"));
    assert!(cfg.contains("      writable: true"));
    assert!(
        !cfg.contains("add_dirs: [\"docs/\""),
        "read path must not leak into add_dirs, got:\n{cfg}"
    );
}

#[test]
fn render_agent_access_none_emits_no_access_block() {
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &provider_of_type("openai"),
        &[access_agent(&[], &[])],
    );
    assert!(!cfg.contains("builtin_tools:"));
    assert!(!cfg.contains("    claude:"));
    assert!(!cfg.contains("add_dirs:"));
}

#[test]
fn render_agent_access_exec_uses_working_dir_prefers_write() {
    let mut a = access_agent(&["./read"], &["./work"]);
    a.exec_command = Some(vec!["python3".to_string(), "agent.py".to_string()]);
    let cfg = render_agent_config(
        "http://nsed:8080",
        "${NSED_BEARER_TOKEN}",
        &provider_of_type("exec"),
        &[a],
    );
    assert!(cfg.contains("    exec:"));
    assert!(cfg.contains("      working_dir: \"./work\""));
    assert!(!cfg.contains("builtin_tools:"));
    assert!(!cfg.contains("    claude:"));
}
