//! `gh` CLI integration for the CI Guardian: a fail-closed preflight and a PR
//! CI-status probe. The pure classification of a check rollup is factored out
//! so it can be unit-tested without a network or a live `gh`.

use serde::Deserialize;

/// Overall CI state of a PR, collapsed from its individual checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiState {
    /// At least one check still running / queued; nothing to act on yet.
    Pending,
    /// Every check succeeded (or was skipped/neutral).
    Passed,
    /// At least one check failed (or errored/cancelled/timed out).
    Failed,
    /// The PR is merged or closed — the watch should stop.
    MergedOrClosed,
}

/// Snapshot of a PR's CI at one poll.
#[derive(Debug, Clone)]
pub struct CiStatus {
    pub head_sha: String,
    pub state: CiState,
    /// Name of the first failing check (dedup key component), when `Failed`.
    pub failing_check: Option<String>,
}

/// Result of the fail-closed startup/poll preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    Ok,
    /// Reason tag: "auth" | "repo-access".
    Blocked(String),
}

#[derive(Deserialize)]
struct CheckItem {
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

enum Cat {
    Pass,
    Fail,
    Pending,
}

/// Categorize one check from its `gh` `bucket` (preferred) or raw `state`.
fn check_category(item: &CheckItem) -> Cat {
    let raw = item
        .bucket
        .as_deref()
        .or(item.state.as_deref())
        .unwrap_or("")
        .to_ascii_uppercase();
    match raw.as_str() {
        "FAIL" | "FAILURE" | "ERROR" | "CANCEL" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" => {
            Cat::Fail
        }
        "PENDING" | "IN_PROGRESS" | "QUEUED" | "WAITING" | "REQUESTED" | "EXPECTED" => Cat::Pending,
        // pass / success / skipping / skipped / neutral / "" -> treat as passing
        _ => Cat::Pass,
    }
}

/// Collapse a `gh pr checks --json ...` (or pr-view rollup) array into one state.
/// Precedence: any failing check → Failed; else any pending → Pending; else
/// Passed. An empty array means checks haven't registered yet → Pending.
pub(crate) fn parse_check_rollup(json: &str) -> anyhow::Result<CiState> {
    let items: Vec<CheckItem> = serde_json::from_str(json)?;
    if items.is_empty() {
        return Ok(CiState::Pending);
    }
    let mut any_fail = false;
    let mut any_pending = false;
    for item in &items {
        match check_category(item) {
            Cat::Fail => any_fail = true,
            Cat::Pending => any_pending = true,
            Cat::Pass => {}
        }
    }
    Ok(if any_fail {
        CiState::Failed
    } else if any_pending {
        CiState::Pending
    } else {
        CiState::Passed
    })
}

/// Name of the first failing check in a rollup, if any.
pub(crate) fn first_failing_name(json: &str) -> Option<String> {
    let items: Vec<CheckItem> = serde_json::from_str(json).ok()?;
    items
        .into_iter()
        .find(|i| matches!(check_category(i), Cat::Fail))
        .and_then(|i| i.name)
}

async fn run_gh(args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(tokio::process::Command::new("gh").args(args).output().await?)
}

/// Fail-closed preflight: confirm `gh` is authenticated and the repo is
/// reachable before any watch does work. On failure returns `Blocked(reason)`
/// so the controller can notify + pause rather than mistake a missing token for
/// "CI still pending".
pub async fn preflight(repo: &str) -> Preflight {
    match run_gh(&["auth", "status"]).await {
        Ok(o) if o.status.success() => {}
        _ => return Preflight::Blocked("auth".into()),
    }
    match run_gh(&["repo", "view", repo, "--json", "name"]).await {
        Ok(o) if o.status.success() => Preflight::Ok,
        _ => Preflight::Blocked("repo-access".into()),
    }
}

/// Probe a PR's current head SHA + CI state. Uses `gh pr view` for the head/
/// open-closed state and `gh pr checks` for the rollup. NOTE: `gh pr checks`
/// exits non-zero when checks are failing, so its stdout is parsed regardless of
/// exit status.
pub async fn probe_pr(repo: &str, pr: u64) -> anyhow::Result<CiStatus> {
    let view = run_gh(&[
        "pr",
        "view",
        &pr.to_string(),
        "--repo",
        repo,
        "--json",
        "state,headRefOid",
    ])
    .await?;
    if !view.status.success() {
        anyhow::bail!(
            "gh pr view failed: {}",
            String::from_utf8_lossy(&view.stderr)
        );
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PrView {
        state: String,
        head_ref_oid: String,
    }
    let pv: PrView = serde_json::from_slice(&view.stdout)?;
    if pv.state == "MERGED" || pv.state == "CLOSED" {
        return Ok(CiStatus {
            head_sha: pv.head_ref_oid,
            state: CiState::MergedOrClosed,
            failing_check: None,
        });
    }

    let checks = run_gh(&[
        "pr",
        "checks",
        &pr.to_string(),
        "--repo",
        repo,
        "--json",
        "bucket,name,state",
    ])
    .await?;
    let stdout = String::from_utf8_lossy(&checks.stdout);
    let state = if stdout.trim().is_empty() {
        // `gh pr checks` exits non-zero both when checks are FAILING (stdout has
        // the rollup) and when it genuinely errors (auth revoked mid-poll,
        // missing scope, no PR) — in the latter stdout is empty. Distinguish:
        // empty stdout + failure = a real error to surface, not "pending".
        if !checks.status.success() {
            anyhow::bail!(
                "gh pr checks failed: {}",
                String::from_utf8_lossy(&checks.stderr)
            );
        }
        // Genuinely no checks registered yet.
        CiState::Pending
    } else {
        parse_check_rollup(&stdout)?
    };
    let failing_check = if state == CiState::Failed {
        first_failing_name(&stdout)
    } else {
        None
    };
    Ok(CiStatus {
        head_sha: pv.head_ref_oid,
        state,
        failing_check,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rollup_classifies_states() {
        assert_eq!(
            parse_check_rollup(r#"[{"state":"SUCCESS"},{"state":"FAILURE"}]"#).unwrap(),
            CiState::Failed
        );
        assert_eq!(
            parse_check_rollup(r#"[{"state":"SUCCESS"}]"#).unwrap(),
            CiState::Passed
        );
        assert_eq!(
            parse_check_rollup(r#"[{"state":"IN_PROGRESS"},{"state":"SUCCESS"}]"#).unwrap(),
            CiState::Pending
        );
        // Empty = checks not registered yet.
        assert_eq!(parse_check_rollup("[]").unwrap(), CiState::Pending);
    }

    #[test]
    fn parse_rollup_prefers_gh_bucket_field() {
        // Real `gh pr checks --json bucket` output uses the normalized bucket.
        assert_eq!(
            parse_check_rollup(r#"[{"bucket":"pass","name":"build"},{"bucket":"fail","name":"test"}]"#)
                .unwrap(),
            CiState::Failed
        );
        assert_eq!(
            parse_check_rollup(r#"[{"bucket":"pass"},{"bucket":"skipping"}]"#).unwrap(),
            CiState::Passed
        );
    }

    #[test]
    fn first_failing_name_returns_the_failing_check() {
        let json = r#"[{"bucket":"pass","name":"build"},{"bucket":"fail","name":"unit-tests"}]"#;
        assert_eq!(first_failing_name(json).as_deref(), Some("unit-tests"));
        let all_pass = r#"[{"bucket":"pass","name":"build"}]"#;
        assert_eq!(first_failing_name(all_pass), None);
    }
}
