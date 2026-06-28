//! Best-effort "newer version available" check against crates.io.
//!
//! Channel-aware: a **stable** user (no pre-release in their version) is only
//! told about newer **stable** releases; an **rc** user is told about any newer
//! version (a newer rc, or the stable that supersedes their rc). Cached for 24h
//! under `~/.nsed/version_check.json` so most invocations make no network call,
//! and **silent on every failure** — it must never block or break the CLI.
//!
//! `cached_notice()` is the instant, cache-only check (printed first); for the
//! final message, `refreshed_notice()` refreshes from crates.io when the cache
//! is stale, then compares.

use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

const CRATE_NAME: &str = "quorum-rs";
const CACHE_TTL_SECS: i64 = 24 * 3600;
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// The version this binary was built as. `None` only if the compile-time
/// `CARGO_PKG_VERSION` is unparseable (shouldn't happen for a real build).
fn current_version() -> Option<Version> {
    Version::parse(env!("CARGO_PKG_VERSION")).ok()
}

/// Pick the newest version appropriate for the caller's release channel:
/// - stable caller (`pre` empty) → only stable candidates are considered
/// - pre-release caller → every candidate (a stable supersedes an rc)
fn latest_for_channel(candidates: &[Version], current: &Version) -> Option<Version> {
    let on_stable = current.pre.is_empty();
    candidates
        .iter()
        .filter(|v| !on_stable || v.pre.is_empty())
        .max()
        .cloned()
}

/// Parse a crates.io `GET /api/v1/crates/{crate}` body into all non-yanked
/// versions. Tolerant: returns an empty vec on any malformed input.
fn parse_versions(body: &str) -> Vec<Version> {
    let json: serde_json::Value = match serde_json::from_str(body) {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    json["versions"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| !v["yanked"].as_bool().unwrap_or(false))
                .filter_map(|v| v["num"].as_str())
                .filter_map(|s| Version::parse(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The upgrade message, or `None` when `latest` isn't newer than `current`.
fn upgrade_notice(current: &Version, latest: &Version) -> Option<String> {
    (latest > current).then(|| {
        format!(
            "\u{2b06} quorum {latest} available (you have {current}) — \
             upgrade: cargo install quorum-rs --version {latest}"
        )
    })
}

#[derive(Serialize, Deserialize)]
struct Cache {
    /// Unix seconds of the last successful crates.io check.
    checked_at: i64,
    /// The channel-appropriate latest version seen at that check.
    latest: String,
}

fn cache_path() -> Option<PathBuf> {
    crate::cli::endpoint::nsed_dir().map(|d| d.join("version_check.json"))
}

fn read_cache() -> Option<Cache> {
    let body = std::fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str(&body).ok()
}

fn write_cache(latest: &str, now: i64) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string(&Cache {
        checked_at: now,
        latest: latest.to_string(),
    }) {
        let _ = std::fs::write(path, body);
    }
}

fn is_fresh(cache: &Cache, now: i64) -> bool {
    now.saturating_sub(cache.checked_at) < CACHE_TTL_SECS
}

/// Instant, cache-only notice — safe to print at startup before doing any work.
/// `None` when no cache, cache not newer, or anything fails.
pub fn cached_notice() -> Option<String> {
    let current = current_version()?;
    let latest = Version::parse(&read_cache()?.latest).ok()?;
    upgrade_notice(&current, &latest)
}

/// Notice for the final message: serve it from a fresh cache, otherwise refresh
/// from crates.io (channel-aware) and update the cache. Best-effort — any
/// failure (offline, timeout, parse) yields `None`. `now` is unix seconds
/// (injected for testability).
pub async fn refreshed_notice(now: i64) -> Option<String> {
    let current = current_version()?;
    if let Some(cache) = read_cache()
        && is_fresh(&cache, now)
        && let Ok(latest) = Version::parse(&cache.latest)
    {
        return upgrade_notice(&current, &latest);
    }
    let versions = fetch_versions().await?;
    let latest = latest_for_channel(&versions, &current)?;
    write_cache(&latest.to_string(), now);
    upgrade_notice(&current, &latest)
}

async fn fetch_versions() -> Option<Vec<Version>> {
    let url = format!("https://crates.io/api/v1/crates/{CRATE_NAME}");
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        // crates.io rejects requests without a descriptive User-Agent.
        .user_agent(concat!("quorum-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let body = client.get(url).send().await.ok()?.text().await.ok()?;
    Some(parse_versions(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn stable_user_only_sees_newer_stable() {
        let candidates = [v("0.6.0"), v("0.7.0-rc.5"), v("0.6.1")];
        // On stable 0.6.0: the rc is ignored; newest stable is 0.6.1.
        let got = latest_for_channel(&candidates, &v("0.6.0")).unwrap();
        assert_eq!(got, v("0.6.1"));
    }

    #[test]
    fn stable_user_ignores_higher_prerelease() {
        let candidates = [v("0.6.0"), v("0.7.0-rc.1")];
        // 0.7.0-rc.1 is numerically higher but pre-release → not offered to stable.
        assert_eq!(
            latest_for_channel(&candidates, &v("0.6.0")).unwrap(),
            v("0.6.0")
        );
    }

    #[test]
    fn rc_user_sees_newer_rc() {
        let candidates = [v("0.6.0-rc.2"), v("0.6.0-rc.16"), v("0.6.0-rc.9")];
        assert_eq!(
            latest_for_channel(&candidates, &v("0.6.0-rc.2")).unwrap(),
            v("0.6.0-rc.16")
        );
    }

    #[test]
    fn rc_user_also_sees_the_superseding_stable() {
        let candidates = [v("0.6.0-rc.16"), v("0.6.0")];
        // A stable release is newer than any of its rc's → offered to rc users.
        assert_eq!(
            latest_for_channel(&candidates, &v("0.6.0-rc.16")).unwrap(),
            v("0.6.0")
        );
    }

    #[test]
    fn notice_only_when_strictly_newer() {
        assert!(upgrade_notice(&v("0.6.0"), &v("0.6.0")).is_none());
        assert!(upgrade_notice(&v("0.6.1"), &v("0.6.0")).is_none());
        let msg = upgrade_notice(&v("0.6.0"), &v("0.6.1")).unwrap();
        assert!(msg.contains("0.6.1") && msg.contains("0.6.0"));
        assert!(msg.contains("cargo install quorum-rs --version 0.6.1"));
    }

    #[test]
    fn parse_versions_skips_yanked_and_malformed() {
        let body = r#"{
            "versions": [
                {"num": "0.6.0", "yanked": false},
                {"num": "0.6.1", "yanked": true},
                {"num": "not-a-version", "yanked": false},
                {"num": "0.7.0-rc.1", "yanked": false}
            ]
        }"#;
        let vs = parse_versions(body);
        assert!(vs.contains(&v("0.6.0")));
        assert!(vs.contains(&v("0.7.0-rc.1")));
        assert!(!vs.contains(&v("0.6.1")), "yanked excluded");
        assert_eq!(vs.len(), 2, "malformed + yanked dropped");
    }

    #[test]
    fn parse_versions_tolerates_garbage() {
        assert!(parse_versions("not json").is_empty());
        assert!(parse_versions("{}").is_empty());
    }

    #[test]
    fn freshness_window() {
        let c = Cache {
            checked_at: 1000,
            latest: "0.6.0".into(),
        };
        assert!(is_fresh(&c, 1000 + CACHE_TTL_SECS - 1));
        assert!(!is_fresh(&c, 1000 + CACHE_TTL_SECS + 1));
    }
}
