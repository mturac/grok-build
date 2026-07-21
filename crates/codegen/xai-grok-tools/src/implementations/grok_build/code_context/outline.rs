//! T1 — file summary: a lead doc/comment plus a language-agnostic outline of
//! top-level definitions. Pure string functions so they are trivially tested.

/// Max definitions listed in a summary (keeps T1 cheap).
const MAX_DEFS: usize = 40;
/// Max characters of lead-comment text kept.
const MAX_LEAD_CHARS: usize = 400;

/// Extract the leading documentation for a file: the first run of comment /
/// docstring / top markdown prose, before any code. Recognizes `//` `///`
/// `//!` (Rust/JS/Go), `#` (Python/shell/YAML/Ruby), and a leading `/* … */`
/// block. Returns a single trimmed line (comment markers stripped), or empty.
pub fn extract_lead_doc(content: &str) -> String {
    let mut lines = content.lines().peekable();
    // Skip a shebang and blank leading lines.
    while let Some(l) = lines.peek() {
        let t = l.trim();
        if t.is_empty() || t.starts_with("#!") {
            lines.next();
        } else {
            break;
        }
    }

    let mut collected: Vec<String> = Vec::new();

    // `/* ... */` block comment.
    if let Some(first) = lines.peek()
        && first.trim_start().starts_with("/*")
    {
        for l in lines.by_ref() {
            let t = l.trim();
            let cleaned = t
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_start_matches('*')
                .trim_end_matches("*/")
                .trim();
            if !cleaned.is_empty() {
                collected.push(cleaned.to_string());
            }
            if t.contains("*/") {
                break;
            }
        }
    } else {
        // Consecutive line comments (// … or # …) or top markdown prose.
        for l in lines.by_ref() {
            let t = l.trim();
            if let Some(rest) = t
                .strip_prefix("//!")
                .or_else(|| t.strip_prefix("///"))
                .or_else(|| t.strip_prefix("//"))
                .or_else(|| t.strip_prefix('#'))
            {
                let r = rest.trim();
                if !r.is_empty() {
                    collected.push(r.to_string());
                }
            } else if collected.is_empty() && !t.is_empty() && !looks_like_code(t) {
                // First non-comment line is prose (e.g. a markdown/plain doc).
                collected.push(t.to_string());
                break;
            } else {
                break;
            }
        }
    }

    let joined = collected.join(" ");
    let joined = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.len() > MAX_LEAD_CHARS {
        let end = floor_char_boundary(&joined, MAX_LEAD_CHARS);
        format!("{}…", &joined[..end])
    } else {
        joined
    }
}

/// Heuristic: does this line look like code rather than prose? Used only to
/// stop lead-doc capture at the first code line in a comment-less file.
fn looks_like_code(line: &str) -> bool {
    const KW: &[&str] = &[
        "import ", "from ", "use ", "package ", "fn ", "def ", "class ", "func ", "const ",
        "let ", "var ", "public ", "private ", "#include", "namespace ", "module ",
    ];
    KW.iter().any(|k| line.starts_with(k))
        || line.ends_with('{')
        || line.ends_with(';')
        || line.starts_with('@')
}

/// Keywords that introduce a top-level definition, checked at the start of a
/// (whitespace-trimmed) line after stripping common visibility modifiers.
const DEF_KEYWORDS: &[&str] = &[
    "fn", "struct", "enum", "trait", "impl", "mod", "type", "const", "static", "macro_rules!",
    "class", "def", "func", "function", "interface", "namespace", "module",
];

/// Modifiers stripped before matching a definition keyword. Note: `const` and
/// `static` are deliberately NOT here — they are themselves definition
/// keywords, so stripping them would hide `const FOO` / `static BAR`.
const MODIFIERS: &[&str] = &[
    "pub", "pub(crate)", "async", "unsafe", "export", "default", "public", "private",
    "protected", "final", "abstract",
];

/// Extract an outline of top-level definitions: `(kind, name)` in file order,
/// de-duplicated, capped. Language-agnostic and line-based — it favors recall
/// (a quick map of a file) over perfect parsing.
pub fn extract_defs(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in content.lines() {
        // Only top-level (unindented) definitions, so nested items (methods,
        // closures, block-scoped fns) don't flood the outline.
        let indent = raw.len() - raw.trim_start().len();
        if indent > 0 {
            continue;
        }
        let mut toks = raw.trim().split_whitespace().peekable();
        // Skip leading modifiers.
        while let Some(t) = toks.peek() {
            if MODIFIERS.contains(t) {
                toks.next();
            } else {
                break;
            }
        }
        let Some(kw) = toks.next() else { continue };
        let kw_norm = kw.trim_end_matches('!');
        if !DEF_KEYWORDS.contains(&kw) && !DEF_KEYWORDS.contains(&kw_norm) {
            continue;
        }
        // The name is the next token, trimmed of punctuation / generics / args.
        let Some(name_tok) = toks.next() else { continue };
        let name: String = name_tok
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let entry = format!("{kw_norm} {name}");
        if !out.contains(&entry) {
            out.push(entry);
        }
        if out.len() >= MAX_DEFS {
            break;
        }
    }
    out
}

/// Build the T1 summary block for a file.
pub fn summarize(path_label: &str, content: &str) -> String {
    let doc = extract_lead_doc(content);
    let defs = extract_defs(content);
    let line_count = content.lines().count();

    let mut out = format!("# {path_label}  ({line_count} lines)\n");
    if !doc.is_empty() {
        out.push_str(&format!("\n{doc}\n"));
    }
    if defs.is_empty() {
        out.push_str("\n(no top-level definitions detected)\n");
    } else {
        out.push_str("\nDefinitions:\n");
        for d in &defs {
            out.push_str(&format!("  - {d}\n"));
        }
        if defs.len() >= MAX_DEFS {
            out.push_str("  … (truncated)\n");
        }
    }
    out
}

/// Largest char boundary `<= idx` (std's is unstable).
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lead_doc_from_rust_module_comment() {
        let src = "//! This module does X.\n//! Second line.\n\nuse std::fmt;\npub fn a() {}";
        assert_eq!(extract_lead_doc(src), "This module does X. Second line.");
    }

    #[test]
    fn lead_doc_from_block_comment_and_hash() {
        assert_eq!(
            extract_lead_doc("/* Top block.\n * more. */\ncode();"),
            "Top block. more."
        );
        assert_eq!(
            extract_lead_doc("#!/bin/sh\n# Deploys the app.\nset -e"),
            "Deploys the app."
        );
    }

    #[test]
    fn lead_doc_empty_when_starts_with_code() {
        assert_eq!(extract_lead_doc("use std::io;\nfn main() {}"), "");
    }

    #[test]
    fn defs_across_languages_and_modifiers() {
        let src = "\
//! doc
pub fn alpha() {}
async fn beta() {}
pub(crate) struct Gamma { x: u8 }
    fn nested_should_be_skipped() {}
enum Delta { A, B }
def py_fn(x):
class Widget:
export function jsThing() {}
";
        let defs = extract_defs(src);
        assert!(defs.contains(&"fn alpha".to_string()));
        assert!(defs.contains(&"fn beta".to_string()));
        assert!(defs.contains(&"struct Gamma".to_string()));
        assert!(defs.contains(&"enum Delta".to_string()));
        assert!(defs.contains(&"def py_fn".to_string()));
        assert!(defs.contains(&"class Widget".to_string()));
        assert!(defs.contains(&"function jsThing".to_string()));
        // Deeply-indented (nested) definitions are excluded from the outline.
        assert!(!defs.iter().any(|d| d.contains("nested_should_be_skipped")));
    }

    #[test]
    fn const_and_static_definitions_are_detected() {
        // Regression: `const`/`static` were both a modifier and a keyword, so
        // the modifier strip hid them. They must be detected, including with a
        // visibility modifier in front.
        let src = "const MAX: usize = 10;\nstatic TABLE: &[u8] = &[];\npub(crate) const K: u8 = 1;\n";
        let defs = extract_defs(src);
        assert!(defs.contains(&"const MAX".to_string()), "got: {defs:?}");
        assert!(defs.contains(&"static TABLE".to_string()), "got: {defs:?}");
        assert!(defs.contains(&"const K".to_string()), "got: {defs:?}");
    }

    #[test]
    fn duplicate_definitions_are_collapsed() {
        let src = "fn foo() {}\nfn foo() {}\n";
        assert_eq!(extract_defs(src), vec!["fn foo".to_string()]);
    }

    #[test]
    fn summarize_composes_doc_and_defs() {
        let src = "//! Widget factory.\npub fn make() {}\npub struct Widget;";
        let s = summarize("src/widget.rs", src);
        assert!(s.contains("# src/widget.rs"));
        assert!(s.contains("Widget factory."));
        assert!(s.contains("- fn make"));
        assert!(s.contains("- struct Widget"));
    }

    #[test]
    fn summarize_handles_no_defs() {
        let s = summarize("notes.txt", "just some prose here\nand more");
        assert!(s.contains("no top-level definitions"));
    }
}
