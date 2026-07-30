//! Checking whether a newer release exists.
//!
//! Two constraints shape this:
//!
//! * **Never block the tool on the network.** `agentsync` and `plan` are local,
//!   file-based, and fast. They read a cache and never make a request; only
//!   `doctor` goes out to the network, because "tell me about problems" is
//!   already the command that costs something.
//! * **No TLS stack.** Pulling in an HTTP client with its own certificate
//!   handling to compare two version numbers is more dependency than the feature
//!   is worth, and it complicates cross-compilation. This shells out to `curl`,
//!   which is the same thing the tool already does for every other side effect —
//!   invoke a program the platform has. Where `curl` is missing, the check
//!   reports that rather than pretending everything is current.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// How long a cached answer stays good. Long enough that the API's
/// unauthenticated rate limit is irrelevant.
const CACHE_TTL_SECS: u64 = 24 * 60 * 60;

/// A refusal to answer is better than a slow command.
const TIMEOUT_SECS: u64 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Running the newest published release.
    Current,
    /// A newer release exists.
    Newer { latest: String },
    /// Running something newer than any release — a local build.
    Ahead { latest: String },
    /// Could not tell. Deliberately distinct from `Current`: a failed check is
    /// not evidence of being up to date.
    Unknown { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cached {
    /// Unix seconds.
    pub checked_at: u64,
    /// The newest tag seen, e.g. `v0.0.4`.
    pub latest: String,
}

pub fn cache_path() -> PathBuf {
    paths::config_dir().join("update-check.json")
}

/// True when the user has asked for no network access.
pub fn offline() -> bool {
    matches!(
        std::env::var("AGENTSYNC_OFFLINE").as_deref(),
        Ok("1") | Ok("true")
    )
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `owner/repo`, derived from the `repository` field in Cargo.toml so a fork
/// checks its own releases rather than ours.
pub fn repo_slug() -> Option<String> {
    let url = option_env!("CARGO_PKG_REPOSITORY")?;
    let rest = url.split("github.com").nth(1)?;
    let slug = rest.trim_start_matches(['/', ':']).trim_end_matches('/');
    let slug = slug.strip_suffix(".git").unwrap_or(slug);
    let mut parts = slug.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

/// Parse `v1.2.3` or `1.2.3`. Anything with a pre-release suffix or a missing
/// component is unparseable rather than guessed at.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v');
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Compare a release tag against the running build.
pub fn compare(current: &str, latest_tag: &str) -> Status {
    let (Some(now), Some(latest)) = (parse_version(current), parse_version(latest_tag)) else {
        return Status::Unknown {
            reason: format!("cannot compare {current:?} with {latest_tag:?}"),
        };
    };
    match latest.cmp(&now) {
        std::cmp::Ordering::Greater => Status::Newer {
            latest: latest_tag.to_string(),
        },
        std::cmp::Ordering::Equal => Status::Current,
        std::cmp::Ordering::Less => Status::Ahead {
            latest: latest_tag.to_string(),
        },
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The last answer, if it is still fresh. Never makes a request.
pub fn cached() -> Option<Cached> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    let cached: Cached = serde_json::from_str(&text).ok()?;
    let age = now_secs().saturating_sub(cached.checked_at);
    (age < CACHE_TTL_SECS).then_some(cached)
}

/// What the cache implies about the running build, without any network access.
/// This is what the TUI shows.
pub fn cached_status() -> Option<Status> {
    let cached = cached()?;
    match compare(current_version(), &cached.latest) {
        Status::Newer { latest } => Some(Status::Newer { latest }),
        _ => None,
    }
}

fn write_cache(latest: &str) -> Result<()> {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cached = Cached {
        checked_at: now_secs(),
        latest: latest.to_string(),
    };
    std::fs::write(&path, serde_json::to_string(&cached)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Ask GitHub for the newest release tag.
fn fetch_latest_tag(slug: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{slug}/releases/latest");
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            &TIMEOUT_SECS.to_string(),
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ])
        .output()
        .context("running curl (needed for the update check)")?;

    if !out.status.success() {
        anyhow::bail!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing the GitHub response")?;
    body.get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .context("the GitHub response had no tag_name")
}

/// Check for a newer release, refreshing the cache. Uses the cache when fresh.
pub fn check() -> Status {
    if offline() {
        return Status::Unknown {
            reason: "AGENTSYNC_OFFLINE is set".into(),
        };
    }
    if let Some(cached) = cached() {
        return compare(current_version(), &cached.latest);
    }
    let Some(slug) = repo_slug() else {
        return Status::Unknown {
            reason: "no GitHub repository is recorded in Cargo.toml".into(),
        };
    };
    match fetch_latest_tag(&slug) {
        Ok(tag) => {
            // A failed cache write must not turn a good answer into an error.
            let _ = write_cache(&tag);
            compare(current_version(), &tag)
        }
        Err(e) => Status::Unknown {
            reason: format!("{e:#}"),
        },
    }
}

/// The command that upgrades a prebuilt install.
pub fn upgrade_hint(latest: &str) -> String {
    match repo_slug() {
        Some(slug) => format!(
            "see https://github.com/{slug}/releases/tag/{latest} \
             (or `cargo install --git https://github.com/{slug} --tag {latest}`)"
        ),
        None => format!("upgrade to {latest}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_with_and_without_the_v() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version(" v0.0.10 "), Some((0, 0, 10)));
    }

    #[test]
    fn refuses_to_guess_at_shapes_it_does_not_understand() {
        // Guessing here would produce a confident wrong answer about whether an
        // upgrade exists, which is worse than saying nothing.
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert_eq!(parse_version("v1.2.3-rc1"), None);
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn compares_by_component_not_lexically() {
        // "0.0.10" < "0.0.9" as strings, which is the bug this guards.
        assert_eq!(
            compare("0.0.9", "v0.0.10"),
            Status::Newer {
                latest: "v0.0.10".into()
            }
        );
        assert_eq!(compare("0.0.3", "v0.0.3"), Status::Current);
        assert_eq!(
            compare("0.1.0", "v0.0.9"),
            Status::Ahead {
                latest: "v0.0.9".into()
            }
        );
    }

    #[test]
    fn an_uncomparable_pair_is_unknown_not_current() {
        assert!(matches!(
            compare("0.0.3", "nightly"),
            Status::Unknown { .. }
        ));
    }

    #[test]
    fn derives_the_slug_from_the_repository_url() {
        // The real value from Cargo.toml, so this fails if that field breaks.
        assert_eq!(repo_slug().as_deref(), Some("bydesign21/open-agent-sync"));
    }

    #[test]
    fn a_stale_cache_is_ignored() {
        let home = crate::testutil::TmpHome::new();
        let dir = home.path();

        let stale = Cached {
            checked_at: now_secs().saturating_sub(CACHE_TTL_SECS + 1),
            latest: "v99.0.0".into(),
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(cache_path(), serde_json::to_string(&stale).unwrap()).unwrap();
        assert!(cached().is_none(), "an expired cache must not be used");

        let fresh = Cached {
            checked_at: now_secs(),
            latest: "v99.0.0".into(),
        };
        std::fs::write(cache_path(), serde_json::to_string(&fresh).unwrap()).unwrap();
        assert_eq!(cached().map(|c| c.latest), Some("v99.0.0".into()));
        // ...and it implies an upgrade is available.
        assert!(matches!(cached_status(), Some(Status::Newer { .. })));
    }
}
