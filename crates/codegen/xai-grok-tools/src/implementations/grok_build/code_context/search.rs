//! T2 search and T3 context assembly over a working directory.
//!
//! Lexical only — no embeddings — so it needs no index and never goes stale:
//! each call walks the tree fresh (respecting `.gitignore` via the `ignore`
//! crate), scores lines against the query terms, and ranks. `tokenize` and
//! `score_line` are pure and unit-tested; the walk is bounded so a large repo
//! cannot make a call unbounded.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;

/// Stop scanning after this many files (keeps a huge tree bounded).
const MAX_FILES: usize = 6000;
/// Skip files larger than this (likely data/generated, not source).
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// Truncate a displayed matching line to this many chars.
const MAX_LINE_LEN: usize = 240;
/// Cap collected hits before ranking (memory bound on pathological trees).
const MAX_HITS: usize = 4000;
/// Lines of context on each side of a hit in T3.
const CONTEXT_WINDOW: usize = 4;

/// A ranked matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Path relative to the search root (forward slashes).
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// The matching line, trimmed and length-capped.
    pub text: String,
    pub score: u32,
}

/// Split a query into lowercase alphanumeric terms, de-duplicated, empties
/// dropped. Underscores split too, so `set_artifact_service` matches `set`,
/// `artifact`, `service`.
pub fn tokenize(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for raw in query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
    {
        let t = raw.to_lowercase();
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    terms
}

/// Score one line (already lowercased) against the terms: the number of term
/// occurrences, with a small whole-word bonus so `fn foo` outranks a mention
/// inside `foobar`.
pub fn score_line(line_lower: &str, terms: &[String]) -> u32 {
    let mut score = 0u32;
    for term in terms {
        let occurrences = line_lower.matches(term.as_str()).count() as u32;
        if occurrences == 0 {
            continue;
        }
        score += occurrences;
        if has_word_boundary_match(line_lower, term) {
            score += 2;
        }
    }
    score
}

/// True if `term` appears in `hay` bounded by non-alphanumeric chars (or ends).
fn has_word_boundary_match(hay: &str, term: &str) -> bool {
    let bytes = hay.as_bytes();
    let mut start = 0;
    while let Some(rel) = hay[start..].find(term) {
        let at = start + rel;
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let after = at + term.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = at + term.len();
    }
    false
}

fn truncate_display(line: &str) -> String {
    let t = line.trim();
    if t.chars().count() > MAX_LINE_LEN {
        let s: String = t.chars().take(MAX_LINE_LEN).collect();
        format!("{s}…")
    } else {
        t.to_string()
    }
}

/// Relative, forward-slashed path of `p` under `root` (falls back to the full
/// path string if `p` is not under `root`).
fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Iterate text files under `root`, bounded, respecting `.gitignore`, invoking
/// `f(relative_path, contents)` for each readable UTF-8 file. The callback
/// returns [`ControlFlow`] so a caller that has collected enough (e.g. hit the
/// hit cap) stops the walk immediately instead of reading every remaining file.
fn for_each_text_file(root: &Path, mut f: impl FnMut(String, String) -> ControlFlow<()>) {
    let mut seen = 0usize;
    for entry in ignore::WalkBuilder::new(root)
        .standard_filters(true)
        // Honor .gitignore even when `root` is not itself a git repo (the
        // `ignore` crate otherwise requires a .git dir before applying it).
        .require_git(false)
        .build()
        .flatten()
    {
        if seen >= MAX_FILES {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Ok(meta) = path.metadata()
            && meta.len() > MAX_FILE_BYTES
        {
            continue;
        }
        // Non-UTF-8 (binary) files fail here and are skipped.
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        seen += 1;
        if f(rel_path(root, path), content).is_break() {
            break;
        }
    }
}

/// Collect ranked hits for `terms` across `root`. Sorted by score desc, then
/// path, then line, for a stable order.
pub fn collect_hits(root: &Path, terms: &[String]) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    if terms.is_empty() {
        return hits;
    }
    for_each_text_file(root, |rel, content| {
        for (i, line) in content.lines().enumerate() {
            if hits.len() >= MAX_HITS {
                // Cap reached — stop the whole walk, don't read more files.
                return ControlFlow::Break(());
            }
            let lower = line.to_lowercase();
            let score = score_line(&lower, terms);
            if score > 0 {
                hits.push(Hit {
                    path: rel.clone(),
                    line: i + 1,
                    text: truncate_display(line),
                    score,
                });
            }
        }
        ControlFlow::Continue(())
    });
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    hits
}

/// T2 — format the top `top_k` ranked lines as `path:line: text`.
pub fn search(root: &Path, query: &str, top_k: usize) -> String {
    let terms = tokenize(query);
    if terms.is_empty() {
        return "No searchable terms in query.".to_string();
    }
    let hits = collect_hits(root, &terms);
    if hits.is_empty() {
        return format!("No matches for {:?}.", query);
    }
    let shown = hits.len().min(top_k);
    let mut out = format!(
        "{} match(es) for {:?} (showing {}):\n",
        hits.len(),
        query,
        shown
    );
    for h in hits.iter().take(top_k) {
        out.push_str(&format!("{}:{}: {}\n", h.path, h.line, h.text));
    }
    out
}

/// T3 — assemble a briefing: the top matches with `CONTEXT_WINDOW` lines of
/// surrounding code, concatenated until roughly `max_tokens` (bytes/4). Each
/// file's lines are read once and cached; overlapping windows in the same file
/// are not repeated.
pub fn build_context(root: &Path, query: &str, max_tokens: usize) -> String {
    let terms = tokenize(query);
    if terms.is_empty() {
        return "No searchable terms in query.".to_string();
    }
    let hits = collect_hits(root, &terms);
    if hits.is_empty() {
        return format!("No matches for {:?}.", query);
    }

    let byte_budget = max_tokens.saturating_mul(4);
    let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();
    // Per-file set of already-emitted line numbers, to skip overlaps.
    let mut emitted: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut out = format!("Context for {:?}:\n", query);

    for h in &hits {
        if out.len() >= byte_budget {
            out.push_str("\n… (budget reached)\n");
            break;
        }
        let lines = file_cache.entry(h.path.clone()).or_insert_with(|| {
            std::fs::read_to_string(root.join(&h.path))
                .map(|c| c.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
        if lines.is_empty() {
            continue;
        }
        let idx = h.line.saturating_sub(1);
        let start = idx.saturating_sub(CONTEXT_WINDOW);
        let end = (idx + CONTEXT_WINDOW + 1).min(lines.len());

        // Skip if this window overlaps one already emitted for the file.
        let ranges = emitted.entry(h.path.clone()).or_default();
        if ranges.iter().any(|(s, e)| start < *e && *s < end) {
            continue;
        }
        ranges.push((start, end));

        out.push_str(&format!("\n── {}:{} ──\n", h.path, h.line));
        for (n, text) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{:>5} | {}\n", start + n + 1, text));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_and_dedups() {
        assert_eq!(tokenize("set_artifact Service"), ["set", "artifact", "service"]);
        assert_eq!(tokenize("foo  foo!!bar"), ["foo", "bar"]);
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn score_rewards_word_boundary() {
        let terms = tokenize("foo");
        // Whole-word match scores higher than a substring-only match.
        assert!(score_line("fn foo() {}", &terms) > score_line("let foobar = 1;", &terms));
        assert_eq!(score_line("nothing here", &terms), 0);
    }

    fn tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/auth.rs"),
            "//! Auth.\npub fn login() {}\nfn helper_login_token() {}\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/db.rs"), "pub fn connect() {}\n").unwrap();
        // Ignored dir must not be searched.
        std::fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join("target/gen.rs"), "fn login() { /* gen */ }\n").unwrap();
        tmp
    }

    #[test]
    fn search_ranks_and_respects_gitignore() {
        let tmp = tree();
        let out = search(tmp.path(), "login", 10);
        assert!(out.contains("src/auth.rs:2"), "got: {out}");
        // The gitignored target/ file must not appear.
        assert!(!out.contains("target/"), "gitignored file leaked: {out}");
    }

    #[test]
    fn search_reports_no_matches() {
        let tmp = tree();
        assert!(search(tmp.path(), "zzznotfound", 10).contains("No matches"));
        assert!(search(tmp.path(), "!!!", 10).contains("No searchable terms"));
    }

    #[test]
    fn context_includes_window_and_line_numbers() {
        let tmp = tree();
        let out = build_context(tmp.path(), "login", 2000);
        assert!(out.contains("── src/auth.rs:2 ──"), "got: {out}");
        assert!(out.contains("2 | pub fn login() {}"), "numbered window: {out}");
    }

    #[test]
    fn hit_cap_bounds_collection() {
        // More matching lines than MAX_HITS: collection stops at the cap (and
        // the walk early-terminates rather than reading on).
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (0..(MAX_HITS + 200))
            .map(|i| format!("let needle_{i} = 1;\n"))
            .collect();
        std::fs::write(tmp.path().join("big.rs"), body).unwrap();
        let hits = collect_hits(tmp.path(), &tokenize("needle"));
        assert_eq!(hits.len(), MAX_HITS, "collection must cap at MAX_HITS");
        assert!(search(tmp.path(), "needle", 5).contains(&format!("{MAX_HITS} match")));
    }

    #[test]
    fn context_respects_token_budget() {
        let tmp = tempfile::tempdir().unwrap();
        // Many matching lines; a tiny budget must truncate.
        let body: String = (0..500).map(|i| format!("let needle_{i} = 1;\n")).collect();
        std::fs::write(tmp.path().join("big.rs"), body).unwrap();
        let out = build_context(tmp.path(), "needle", 50);
        assert!(out.contains("budget reached"), "must truncate: len={}", out.len());
    }
}
