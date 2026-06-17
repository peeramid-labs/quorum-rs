use std::path::Path;
use std::process::ExitCode;

use crate::cli::workspace::WorkspaceConfig;

use super::common::{build_remote, resolve_orchestrators};

/// `quorum rooms` — list the rooms you can submit to. The orchestrator
/// returns its grant-filtered view (`GET /rooms`): public rooms plus any
/// whose tags glob-match your token's grants.
pub async fn run(config_path: &Path, orchestrator: Option<&str>) -> ExitCode {
    let config = match WorkspaceConfig::load_or_remote_default(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let targets = match resolve_orchestrators(&config, orchestrator) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut any_failed = false;

    for (name, orch) in &targets {
        let client = match build_remote(name, orch) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                any_failed = true;
                continue;
            }
        };

        let address = orch.address.as_deref().unwrap_or("?");
        println!("Orchestrator: {name} ({address})");

        match client.discover_rooms().await {
            Ok(rooms) => {
                if rooms.is_empty() {
                    println!("  Rooms: (none available to you)");
                } else {
                    println!("  Rooms ({}):", rooms.len());
                    for r in &rooms {
                        let tags = if r.tags.is_empty() {
                            "-".to_string()
                        } else {
                            r.tags.join(",")
                        };
                        println!(
                            "    {:<24} {:<9} {} agent(s)  tags: {tags}",
                            r.id, r.visibility, r.eligible_agent_count
                        );
                    }
                }
            }
            Err(e) => {
                println!("  Rooms: failed to fetch ({e})");
                any_failed = true;
            }
        }
    }

    if any_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end config-free wiring: with no `nsed.yaml`, `quorum rooms`
    /// must synthesize the orchestrator from `~/.nsed/{orchestrator,
    /// operator.token}` and hit the orchestrator's grant-filtered
    /// `GET /rooms` with the persisted bearer.
    ///
    /// `run` is async and reads `$HOME` while it executes, so the env
    /// override must span the whole `.await` (not just future creation).
    /// Serialised on `home_env` so concurrent tests don't race the env.
    #[tokio::test]
    #[serial_test::serial(home_env)]
    async fn config_free_rooms_discovers_against_persisted_endpoint() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rooms"))
            .and(header("authorization", "Bearer home-bearer"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": "noosphera",
                    "tags": ["noosphera:0v1"],
                    "visibility": "public",
                    "eligible_agent_count": 2,
                    "eligible_agent_ids": ["a", "b"],
                }])),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let home = tempfile::TempDir::new().unwrap();
        let nsed = home.path().join(".nsed");
        std::fs::create_dir_all(&nsed).unwrap();
        std::fs::write(nsed.join("orchestrator"), format!("{}\n", mock.uri())).unwrap();
        std::fs::write(nsed.join("operator.token"), "home-bearer\n").unwrap();

        let prev_home = std::env::var_os("HOME");
        // SAFETY: serialised via `serial_test::serial(home_env)`; the env
        // override is held across the whole await below.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::remove_var("QUORUM_ORCHESTRATOR");
        }

        // Config path deliberately does not exist → config-free path.
        let missing = home.path().join("nsed.yaml");
        let _code = run(&missing, None).await;

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        // `.expect(1)` on the mock asserts the correctly-authed GET /rooms
        // fired — i.e. the synthetic orchestrator resolved end to end.
        let reqs = mock.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "exactly one GET /rooms expected");
    }
}
