//! End-to-end for the **third-party custom-provider path**.
//!
//! This is the reference a `codex` (or any external CLI-agent) provider
//! crate follows — it exercises the exact loop such a crate uses, with no
//! SDK changes:
//!
//! 1. read bespoke config from the generic `provider_config` map via
//!    [`AgentConfig::provider_config_as`];
//! 2. build an [`NsedAgent`] that reuses the SDK's public
//!    [`cli_base::spawn_child`] / [`cli_base::effective_timeout`];
//! 3. register a [`ProviderFactory`] on a [`ProviderRegistry`];
//! 4. dispatch through [`build_worker`] — the same path `serve_fleet` drives.
//!
//! Run (needs a NATS broker for the worker test):
//! `NATS_URL=nats://localhost:4222 cargo test -p quorum-rs --test custom_provider_e2e`

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use quorum_rs::agents::config::AgentConfig;
use quorum_rs::agents::{AgentContext, Evaluation, NsedAgent, Proposal};
use quorum_rs::config::{AgentFleetConfig, ProviderEntry, load_agent_from_config_with_registry};
use quorum_rs::providers::{ProviderFactory, ProviderRegistry, cli_base};
use quorum_rs::serve::build_worker;

/// Bespoke config the custom provider reads out of `provider_config` —
/// the third party owns this type; the SDK never sees it.
#[derive(Debug, Clone, serde::Deserialize)]
struct DemoCliConfig {
    /// Argv of the CLI to run.
    command: Vec<String>,
    #[serde(default)]
    permission_mode: String,
}

/// A minimal CLI-agent: spawns `command` through the SDK's shared
/// [`cli_base`] plumbing and returns its stdout as the proposal content.
#[derive(Debug, Clone)]
struct DemoCliAgent {
    name: String,
    config: DemoCliConfig,
}

#[async_trait]
impl NsedAgent for DemoCliAgent {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn propose(&self, ctx: &AgentContext) -> Result<Proposal> {
        let timeout = cli_base::effective_timeout(None, ctx);
        let mut child = cli_base::spawn_child(
            "demo_cli",
            &self.name,
            &self.config.command,
            None,
            &HashMap::new(),
            &[("DEMO_PERMISSION_MODE", self.config.permission_mode.clone())],
        )?;
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut out = String::new();
        tokio::time::timeout(timeout, async {
            tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut out).await?;
            child.wait().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(Proposal {
            content: out.trim().to_string(),
            ..Default::default()
        })
    }

    async fn evaluate(&self, _ctx: &AgentContext) -> Result<Vec<(String, Evaluation)>> {
        Ok(vec![])
    }
}

/// The provider factory — registered for `provider.type: demo_cli`.
struct DemoCliFactory;

impl ProviderFactory for DemoCliFactory {
    fn provider_type(&self) -> &str {
        "demo_cli"
    }

    fn build_agent(
        &self,
        agent_config: &AgentConfig,
        _provider: &ProviderEntry,
    ) -> Result<Option<Arc<dyn NsedAgent>>> {
        let config: DemoCliConfig = agent_config.provider_config_as()?;
        Ok(Some(Arc::new(DemoCliAgent {
            name: agent_config.name.clone(),
            config,
        })))
    }
}

fn demo_fleet() -> AgentFleetConfig {
    serde_yaml::from_str(
        r#"
providers:
  my_codex:
    type: demo_cli
agents:
  - name: DEMO_CLI_A
    provider_id: my_codex
    model_name: codex-mini
    provider_config:
      command: ["/bin/sh", "-c", "printf 'hello from %s' \"$DEMO_PERMISSION_MODE\""]
      permission_mode: "auto"
"#,
    )
    .expect("fleet yaml must parse")
}

/// Registry-aware config load: the custom `demo_cli` provider has **no
/// `api_key`** in its YAML, yet validation passes because its factory
/// reports `requires_api_key() == false` (a local CLI provider).
fn resolve_with_demo_registry() -> (AgentConfig, ProviderEntry) {
    let fleet = demo_fleet();
    let mut registry = ProviderRegistry::with_builtins();
    registry.register(Arc::new(DemoCliFactory));
    load_agent_from_config_with_registry(&fleet, "DEMO_CLI_A", &registry)
        .expect("keyless custom local provider must pass validation via registry.is_local")
}

/// No NATS: provider_config → factory → agent → propose() through the
/// public `cli_base` spawn helper. Proves the third-party building blocks.
#[tokio::test]
async fn custom_provider_propose_via_provider_config_and_cli_base() {
    let (cfg, provider) = resolve_with_demo_registry();

    let agent = DemoCliFactory
        .build_agent(&cfg, &provider)
        .unwrap()
        .expect("custom factory must build an agent from provider_config");
    assert_eq!(agent.name(), "DEMO_CLI_A");

    let proposal = agent.propose(&AgentContext::default()).await.unwrap();
    // The spawned CLI saw the provider_config `permission_mode` via env and
    // echoed it — proving config flowed through provider_config → cli_base.
    assert_eq!(proposal.content, "hello from auto");
}

/// NATS: register the custom factory and dispatch through `build_worker` —
/// the live worker-construction path `serve_fleet` uses for every agent.
#[tokio::test]
async fn custom_provider_builds_worker_over_nats() {
    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());

    let fleet = demo_fleet();
    let mut registry = ProviderRegistry::with_builtins();
    registry.register(Arc::new(DemoCliFactory));

    let built = build_worker(
        &fleet,
        "DEMO_CLI_A",
        &nats_url,
        None,
        "sphera_jobs",
        "sphera",
        &registry,
    )
    .await
    .expect("build_worker must not error for a registered custom provider");

    assert!(
        built.is_some(),
        "registry must dispatch `demo_cli` to a live NATS worker"
    );
}
