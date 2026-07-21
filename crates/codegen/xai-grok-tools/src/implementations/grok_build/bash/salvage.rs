//! Salvage the signal from truncated command output.
//!
//! When a command's output is large, the bash tool keeps only the first and
//! last slices and writes the full output to a file. For a build or test log
//! the actual failure is often in the *middle* — lost to both the head and the
//! tail. This module scans the full output for error/failure (and, with lower
//! priority, warning) lines that are NOT already visible in the shown
//! head/tail, so the model still sees why the command failed without reading
//! the whole file back.
//!
//! Pure string functions — no IO — so they are trivially tested; the caller
//! supplies the full text (read from the output file) and the shown text.

use std::collections::HashSet;

use strip_ansi_escapes::strip_str;

/// High-priority markers: a line containing one (case-insensitive, at a left
/// word boundary) is treated as an error/failure worth surfacing.
const ERROR_MARKERS: &[&str] = &[
    "error", "failed", "failure", "fail", "panic", "fatal", "exception", "traceback",
    "assertion", "assert failed", "segfault", "segmentation fault", "undefined reference",
    "cannot find", "not found", "unresolved", "abort",
];

/// Low-priority markers: surfaced only after errors, to fill remaining budget.
const WARN_MARKERS: &[&str] = &["warning", "warn:", "deprecated"];

/// True if `needle` occurs in `hay` at a **left** word boundary — the char
/// before the match is non-alphanumeric (or the match is at the start). This
/// rejects mid-word noise (`terror` ~ `error`) while deliberately still
/// matching suffix forms (`errors`, `failed`, `panicked`, `failures`): for a
/// failure salvage, recall (never miss the real failure) matters more than
/// precision, and a stray benign line is cheap noise.
fn contains_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
    })
}

fn matches_any(line_lower: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| contains_word(line_lower, m))
}

/// Extract salvaged error/warning lines present in `full` but not already in
/// `shown`. Returns a formatted block (each line as `L<n>: <text>`), or `None`
/// when the output was not truncated-away (nothing new to surface).
///
/// - `full` / `shown` may contain ANSI escapes; both are stripped internally.
/// - Errors are collected first, then warnings, up to `max_lines` / `max_chars`.
/// - De-duplicated by trimmed text (against `shown` and within the result).
pub fn salvage_error_lines(
    full: &str,
    shown: &str,
    max_lines: usize,
    max_chars: usize,
) -> Option<String> {
    if max_lines == 0 || max_chars == 0 {
        return None;
    }
    // Lines already visible to the model (trimmed, ANSI-stripped, lowercased)
    // — never re-surface these. Lowercased so casing variants dedup.
    let shown_set: HashSet<String> = strip_str(shown)
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect();

    let stripped_full = strip_str(full);

    // Collect candidates with a priority tag (0 = error, 1 = warning). Dedup is
    // case-insensitive (lowercased key) while the original text is displayed.
    let mut picked: Vec<(u8, usize, String)> = Vec::new();
    let mut seen_text: HashSet<String> = HashSet::new();

    for (priority, markers) in [(0u8, ERROR_MARKERS), (1u8, WARN_MARKERS)] {
        for (idx, raw_line) in stripped_full.lines().enumerate() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if !matches_any(&lower, markers) {
                continue;
            }
            // Skip anything already shown, or already picked (e.g. matched in
            // the error pass, don't re-pick in the warning pass) — by
            // lowercased key so `ERROR: x` and `error: x` collapse.
            if shown_set.contains(&lower) || !seen_text.insert(lower) {
                continue;
            }
            picked.push((priority, idx + 1, trimmed.to_string()));
        }
    }

    if picked.is_empty() {
        return None;
    }

    // Errors before warnings, each group in document order. Sorting by
    // (priority, line) BEFORE the char-budget loop guarantees errors get the
    // budget first — a late-line error can't be crowded out by an early-line
    // warning.
    picked.sort_by_key(|(priority, line, _)| (*priority, *line));

    let mut out = String::from("[key lines from elided output]\n");
    let mut used = out.len();
    let mut emitted = 0usize;
    for (_, n, text) in picked.iter().take(max_lines) {
        let line = format!("  L{n}: {text}\n");
        if used + line.len() > max_chars {
            break;
        }
        out.push_str(&line);
        used += line.len();
        emitted += 1;
    }
    if emitted == 0 { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_error_from_elided_middle() {
        let full = "line1 ok\nline2 ok\nERROR: boom happened\nline4 ok\nline5 ok";
        // Head/tail shown skipped the middle error line.
        let shown = "line1 ok\nline2 ok\nline5 ok";
        let out = salvage_error_lines(full, shown, 20, 1500).unwrap();
        assert!(out.contains("[key lines from elided output]"));
        assert!(out.contains("L3: ERROR: boom happened"), "got: {out}");
    }

    #[test]
    fn none_when_error_already_shown() {
        let full = "ok\nERROR: boom\nok";
        let shown = "ok\nERROR: boom\nok"; // error already visible
        assert_eq!(salvage_error_lines(full, shown, 20, 1500), None);
    }

    #[test]
    fn none_when_no_signal_lines() {
        let full = "all\nfine\nhere\nnothing to see";
        assert_eq!(salvage_error_lines(full, "all", 20, 1500), None);
    }

    #[test]
    fn errors_prioritized_over_warnings_under_line_cap() {
        let full = "warning: w1\nwarning: w2\nERROR: e1\nwarning: w3";
        // max_lines = 1 → the error must win the single slot.
        let out = salvage_error_lines(full, "", 1, 1500).unwrap();
        assert!(out.contains("ERROR: e1"), "error must win: {out}");
        assert!(!out.contains("w1"));
    }

    #[test]
    fn strips_ansi_before_matching_and_dedups() {
        let full = "\x1b[31mERROR: red boom\x1b[0m\nERROR: red boom";
        // ANSI-stripped text collapses to one line; dedup keeps a single entry.
        let out = salvage_error_lines(full, "", 20, 1500).unwrap();
        assert_eq!(out.matches("ERROR: red boom").count(), 1, "got: {out}");
    }

    #[test]
    fn respects_char_budget() {
        let full: String = (0..50).map(|i| format!("error number {i}\n")).collect();
        let out = salvage_error_lines(&full, "", 50, 60).unwrap();
        // Tiny char budget → only the header plus a line or two fit.
        assert!(out.len() <= 60 + 40, "budget roughly respected: len={}", out.len());
    }

    #[test]
    fn dedups_case_insensitive() {
        // Same message in different casing must surface once.
        let full = "ERROR: boom\nerror: boom";
        let out = salvage_error_lines(full, "", 20, 1500).unwrap();
        assert_eq!(
            out.matches("boom").count(),
            1,
            "casing variants must dedup: {out}"
        );
    }

    #[test]
    fn surfaces_warnings_when_no_errors() {
        let full = "ok\nwarning: deprecated thing\nok";
        let out = salvage_error_lines(full, "ok", 20, 1500).unwrap();
        assert!(out.contains("warning: deprecated thing"), "got: {out}");
    }

    #[test]
    fn zero_limits_return_none() {
        let full = "ERROR: boom";
        assert_eq!(salvage_error_lines(full, "", 0, 1500), None);
        assert_eq!(salvage_error_lines(full, "", 20, 0), None);
    }

    #[test]
    fn word_boundary_reduces_false_positives() {
        // "error" inside "terror" / "mirror-like" must NOT match; a real
        // "error:" line must. (Suffix forms like "errors" still match — recall.)
        let full = "the terror of debugging\nnothing here\nerrors: 3 found";
        let out = salvage_error_lines(full, "", 20, 1500).unwrap();
        assert!(!out.contains("terror"), "mid-word must not match: {out}");
        assert!(out.contains("errors: 3 found"), "suffix form matches: {out}");
    }

    #[test]
    fn char_budget_prefers_error_over_earlier_warning() {
        // An early-line warning and a late-line error; budget fits ONE line.
        // The error must win despite appearing later in the file.
        let full = "warning: early warn\nfiller\nfiller\nERROR: late boom";
        let out = salvage_error_lines(full, "", 20, 55).unwrap();
        assert!(out.contains("ERROR: late boom"), "error must win budget: {out}");
        assert!(!out.contains("early warn"), "warning must yield: {out}");
    }
}
