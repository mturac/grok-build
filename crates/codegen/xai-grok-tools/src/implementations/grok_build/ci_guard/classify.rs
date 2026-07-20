//! Diagnosis-first failure classification.
//!
//! Only a `ConfidentCode` result (with cited failing tests / a concrete panic or
//! assertion) unlocks the autonomous fix path. Everything else — flaky, infra,
//! timeout, permission, or anything unrecognized — stops at "cannot auto-fix".
//!
//! SAFETY PRECEDENCE: the non-fixable categories are checked BEFORE the
//! confident-code signal. This is deliberate: a false negative (skipping a
//! genuinely fixable failure) is safe, but a false positive (attempting a code
//! fix for an infra/timeout/permission failure) is not. When in doubt we skip.

/// The outcome of diagnosing a CI failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureClass {
    /// A retried/flaky check — not a real failure.
    Flaky,
    /// Runner/host/infrastructure problem, not the code.
    Infra,
    /// A timeout / deadline — usually transient, not a code bug to patch.
    Timeout,
    /// Auth/permission/secret problem — outside what a code fix can address.
    Permission,
    /// Recognized as a failure but not confidently a localized code bug.
    Ambiguous,
    /// A confident, localized code failure with concrete citations. The ONLY
    /// class that unlocks the fix path.
    ConfidentCode { failing_tests: Vec<String> },
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Extract Rust-style failing test names from `test <name> ... FAILED` lines.
/// Case-sensitive on purpose (`FAILED` is upper-case in cargo output).
fn extract_failing_tests(log: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in log.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("test ")
            && let Some(idx) = rest.find(" ... ")
        {
            let (name, tail) = rest.split_at(idx);
            if tail.contains("FAILED") {
                out.push(name.trim().to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Classify a failure from the tail of its logs plus any check annotations.
pub fn classify(log_tail: &str, annotations: &[String]) -> FailureClass {
    let mut hay = log_tail.to_ascii_lowercase();
    for a in annotations {
        hay.push('\n');
        hay.push_str(&a.to_ascii_lowercase());
    }

    // --- Non-fixable categories FIRST (safety precedence). ---
    // Infra / runner / host / transient-service failures — never a code fix.
    if contains_any(
        &hay,
        &[
            "runner has received a shutdown",
            "the runner has received",
            "runner disconnected",
            "agent disconnected",
            "runner not found",
            "lost communication with the server",
            "lost communication with the runner",
            "deprovision",
            "no space left on device",
            "cannot allocate memory",
            "out of memory",
            "oomkilled",
            "connection reset",
            "connection refused",
            "no such host",
            "no route to host",
            "temporary failure in name resolution",
            "rate limit exceeded",
            "api rate limit",
            "502 bad gateway",
            "503 service",
            "504 gateway",
            "job was cancelled",
            "job cancelled",
            "the operation was canceled",
            "the operation was cancelled",
        ],
    ) {
        return FailureClass::Infra;
    }
    if contains_any(
        &hay,
        &[
            "timed out",
            "timeout",
            "deadline exceeded",
            "etimedout",
            "context deadline",
        ],
    ) {
        return FailureClass::Timeout;
    }
    if contains_any(
        &hay,
        &[
            "permission to",
            "permission denied",
            "403 forbidden",
            "forbidden",
            "authentication failed",
            "denied to",
            "not authorized",
            "unauthorized",
            "401",
            "bad credentials",
            "token expired",
            "resource not accessible by integration",
        ],
    ) {
        return FailureClass::Permission;
    }
    if contains_any(
        &hay,
        &["flaky", "retrying", "rerun", "attempt 2", "attempt #2", "re-run"],
    ) {
        return FailureClass::Flaky;
    }

    // --- Confident code failure ONLY on a concrete code fingerprint. ---
    // A bare `test X ... FAILED` line is NOT enough: infra/network smoke tests
    // print "FAILED" too, and any such log whose infra phrasing escaped the
    // lists above must still fall through to Ambiguous (skip), not unlock a
    // fix. Require an actual panic / assertion / compiler diagnostic. Failing
    // test names are attached as context (branch name, notification) only.
    let has_code_symptom = contains_any(
        &hay,
        &[
            "assertion `left",
            "assertion failed",
            "panicked",        // covers "panicked at" / "thread '..' panicked at"
            "error[e",         // rustc diagnostic code, e.g. error[E0308]
            "cannot find value",
            "cannot find function",
            "mismatched types",
            "borrow of moved value",
        ],
    );
    if has_code_symptom {
        let failing_tests = extract_failing_tests(log_tail);
        return FailureClass::ConfidentCode { failing_tests };
    }

    FailureClass::Ambiguous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_table() {
        assert!(matches!(
            classify("test math::add ... FAILED\nassertion `left == right`", &[]),
            FailureClass::ConfidentCode { .. }
        ));
        assert!(matches!(
            classify("Error: The runner has received a shutdown signal", &[]),
            FailureClass::Infra
        ));
        assert!(matches!(
            classify("error: failed to run custom build command (network timeout)", &[]),
            FailureClass::Timeout
        ));
        assert!(matches!(
            classify("remote: Permission to repo denied", &[]),
            FailureClass::Permission
        ));
        assert!(matches!(
            classify("something totally unrecognized", &[]),
            FailureClass::Ambiguous
        ));
    }

    #[test]
    fn confident_code_extracts_test_names() {
        // Realistic cargo output: the failing test prints its panic/assertion.
        let log = "running 2 tests\ntest core::parse ... ok\ntest core::eval ... FAILED\n\
                   \nthread 'core::eval' panicked at src/core.rs:42:\nassertion `left == right` failed\n\
                   \nfailures:\n    core::eval\n";
        match classify(log, &[]) {
            FailureClass::ConfidentCode { failing_tests } => {
                assert_eq!(failing_tests, vec!["core::eval".to_string()]);
            }
            other => panic!("expected ConfidentCode, got {other:?}"),
        }
    }

    #[test]
    fn bare_failed_line_without_code_symptom_is_ambiguous() {
        // A "FAILED" with NO panic/assertion/compiler fingerprint must NOT
        // unlock a fix — this is the safety gate against infra/network smoke
        // tests whose phrasing escaped the deny-lists.
        let log = "test connectivity::reach_api ... FAILED\nExpected 200, got 000\n";
        assert!(matches!(classify(log, &[]), FailureClass::Ambiguous));
    }

    #[test]
    fn infra_wins_over_incidental_test_failure() {
        // A log with BOTH a runner-shutdown AND a FAILED+panic line must still
        // classify as Infra (safety): we never attempt a code fix when infra is
        // implicated, even with a real-looking code symptom present.
        let log = "test core::eval ... FAILED\nthread panicked at assertion failed\n\
                   The runner has received a shutdown signal";
        assert!(matches!(classify(log, &[]), FailureClass::Infra));
    }

    #[test]
    fn escaped_infra_phrasing_plus_bare_failed_stays_safe() {
        // Even if an infra phrasing is NOT in our lists, a bare FAILED without a
        // code symptom falls through to Ambiguous, not ConfidentCode.
        let log = "test smoke::ping ... FAILED\nsome-unlisted-infra-thing happened\n";
        assert!(matches!(classify(log, &[]), FailureClass::Ambiguous));
    }

    #[test]
    fn annotations_are_considered() {
        assert!(matches!(
            classify("build step failed", &["Permission denied to push".to_string()]),
            FailureClass::Permission
        ));
    }
}
