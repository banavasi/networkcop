//! Update check against crates.io.
//!
//! Runs in the background, never blocks startup, and never fails a session:
//! offline, rate-limited, or crates.io being down all resolve to "no news".
//! Opt out with `--no-update-check` or `NETWORKCOP_NO_UPDATE_CHECK=1`.

use anyhow::{Context, Result};
use std::time::Duration;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const REGISTRY: &str = "https://crates.io/api/v1/crates/networkcop";
const TIMEOUT: Duration = Duration::from_secs(4);

/// The command that installs the newest release.
pub const UPDATE_COMMAND: &str = "cargo install networkcop --force";

/// Newest stable version on crates.io.
pub async fn latest() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // crates.io rejects requests without a descriptive User-Agent
        .user_agent(format!(
            "networkcop/{CURRENT} (+https://github.com/banavasi/networkcop)"
        ))
        .build()?;
    let v: serde_json::Value = client
        .get(REGISTRY)
        .send()
        .await
        .context("query crates.io")?
        .error_for_status()?
        .json()
        .await?;
    v["crate"]["max_stable_version"]
        .as_str()
        .map(|s| s.to_string())
        .context("crates.io response had no max_stable_version")
}

/// `Some(newer)` when an upgrade exists. `None` for up-to-date, or for any
/// failure — an update check must never be the reason a debugging session stops.
pub async fn check() -> Option<String> {
    if std::env::var_os("NETWORKCOP_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let latest = latest().await.ok()?;
    is_newer(&latest, CURRENT).then_some(latest)
}

/// What to tell the user when a newer version exists.
pub fn announcement(latest: &str) -> String {
    format!(
        "networkcop {latest} is available (you have {CURRENT}) — update with:\n  {UPDATE_COMMAND}"
    )
}

/// Compare two `x.y.z` versions.
///
/// crates.io `max_stable_version` is always a plain semver triple, so a
/// three-integer compare is sufficient and avoids a dependency. Anything
/// unparseable sorts as "not newer" — a bad parse must not nag the user.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (triple(candidate), triple(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn triple(v: &str) -> Option<(u64, u64, u64)> {
    // tolerate a leading `v` and any -pre/+build suffix
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() {
        return None; // 1.2.3.4 is not a version we understand
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_newer_release() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.0"), "numeric, not lexical");
    }

    #[test]
    fn same_or_older_is_not_an_update() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn tolerates_v_prefix_and_suffixes() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.2.0-rc.1", "0.1.0"));
        assert!(!is_newer("0.1.0+build9", "0.1.0"));
    }

    #[test]
    fn unparseable_versions_never_nag() {
        for bad in ["", "latest", "1.2.3.4", "x.y.z", "0.1.abc"] {
            assert!(!is_newer(bad, "0.1.0"), "{bad} must not report an update");
            assert!(
                !is_newer("9.9.9", bad),
                "unparseable current is not an update"
            );
        }
    }

    #[test]
    fn short_versions_fill_in_zeros() {
        assert_eq!(triple("1"), Some((1, 0, 0)));
        assert_eq!(triple("1.2"), Some((1, 2, 0)));
        assert!(is_newer("2", "1.9.9"));
    }

    #[test]
    fn announcement_names_both_versions_and_the_command() {
        let a = announcement("9.9.9");
        assert!(a.contains("9.9.9"));
        assert!(a.contains(CURRENT));
        assert!(a.contains("cargo install networkcop --force"));
    }

    #[test]
    fn current_is_the_compiled_package_version() {
        assert!(
            triple(CURRENT).is_some(),
            "CARGO_PKG_VERSION must parse: {CURRENT}"
        );
    }
}
