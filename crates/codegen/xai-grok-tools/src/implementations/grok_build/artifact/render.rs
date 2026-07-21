//! Rendering artifacts to servable HTML.
//!
//! Markdown is rendered to a minimal, theme-aware HTML page. Raw HTML is
//! served as-is. Either way the serving route attaches [`ARTIFACT_CSP`], a
//! strict Content-Security-Policy that makes an artifact self-contained: no
//! external hosts and, crucially, `connect-src 'none'` so a served artifact
//! cannot call back to the agent's WebSocket or exfiltrate the server secret.

use pulldown_cmark::{Options, Parser, html};

use super::store::{Artifact, ArtifactMeta};
use super::types::ArtifactFormat;

/// Strict CSP for served artifacts. Artifacts are hosted on the SAME origin as
/// the authenticated `/ws` endpoint and the PWA (which keeps the server secret
/// in that origin's storage), so an artifact — potentially produced via prompt
/// injection — must not be able to read that storage or exfiltrate it.
///
/// Two layers close that hole:
/// - `sandbox` with no allow-tokens forces the document into a unique opaque
///   origin, so it cannot touch the real origin's `localStorage`/cookies, and
///   blocks scripts, forms, popups, and top-level navigation (the escape hatch
///   `connect-src` alone does not cover).
/// - `script-src 'none'` and `connect-src 'none'` are belt-and-suspenders: no
///   script runs and no network egress is possible even if a UA mis-handles
///   the sandbox.
///
/// Consequence: artifacts are STATIC (HTML + inline CSS + `data:` images). This
/// covers reports, tables, and summaries; interactive JS is intentionally not
/// supported (it would require a separate sandbox origin, as Claude does).
pub const ARTIFACT_CSP: &str = "default-src 'none'; \
img-src 'self' data:; \
style-src 'unsafe-inline'; \
font-src data:; \
script-src 'none'; \
connect-src 'none'; \
frame-src 'none'; \
object-src 'none'; \
base-uri 'none'; \
form-action 'none'; \
sandbox";

/// Escape the five XML/HTML special characters for safe insertion into text
/// and double-quoted attribute contexts.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Minimal theme-aware page CSS. Kept inline so the page is self-contained
/// under the CSP (no external stylesheet).
const PAGE_CSS: &str = "\
:root{color-scheme:light dark}\
*{box-sizing:border-box}\
body{margin:0;padding:2rem 1rem;font:16px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\
background:#fff;color:#1a1a1a}\
main{max-width:44rem;margin:0 auto}\
h1,h2,h3{line-height:1.25;margin:1.6em 0 .5em}\
h1{margin-top:0}\
a{color:#0969da}\
pre{background:#f6f8fa;padding:1rem;border-radius:8px;overflow-x:auto}\
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}\
pre code{background:none;padding:0}\
:not(pre)>code{background:#f6f8fa;padding:.15em .35em;border-radius:4px}\
table{border-collapse:collapse;width:100%;overflow-x:auto;display:block}\
th,td{border:1px solid #d0d7de;padding:.4em .7em}\
blockquote{margin:0;padding:0 1em;border-left:.25em solid #d0d7de;color:#57606a}\
img{max-width:100%}\
@media(prefers-color-scheme:dark){\
body{background:#0d1117;color:#e6edf3}\
a{color:#4493f8}\
pre,:not(pre)>code{background:#161b22}\
th,td{border-color:#30363d}\
blockquote{border-color:#30363d;color:#8b949e}}";

/// Render a Markdown body into a complete, styled HTML page titled `title`.
pub fn render_markdown(title: &str, markdown: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut body = String::new();
    html::push_html(&mut body, Parser::new_ext(markdown, opts));
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <style>{PAGE_CSS}</style>\n\
         </head>\n<body>\n<main>\n{body}</main>\n</body>\n</html>\n",
        title = html_escape(title),
    )
}

/// Render the `/artifacts` index: a styled page linking to each artifact,
/// newest first. Ids are `[A-Za-z0-9_-]` (validated at publish), so they are
/// safe in an href; titles are escaped.
pub fn render_index(rows: &[ArtifactMeta]) -> String {
    let mut items = String::new();
    if rows.is_empty() {
        items.push_str("<li><em>No artifacts published yet.</em></li>");
    } else {
        for m in rows {
            items.push_str(&format!(
                "<li><a href=\"/artifacts/{id}\">{title}</a> <small>{fmt}</small></li>\n",
                id = m.id,
                title = html_escape(&m.title),
                fmt = m.format.ext(),
            ));
        }
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Artifacts</title>\n\
         <style>{PAGE_CSS}\nli{{margin:.35em 0}}small{{color:#8b949e}}</style>\n\
         </head>\n<body>\n<main>\n<h1>Artifacts</h1>\n<ul>\n{items}</ul>\n</main>\n</body>\n</html>\n"
    )
}

/// Produce the HTML to serve for an artifact. Markdown is wrapped in a styled
/// page; raw HTML is returned verbatim (the route still applies the CSP
/// header). The returned string is always served as `text/html`.
pub fn render(artifact: &Artifact) -> String {
    match artifact.format {
        ArtifactFormat::Markdown => render_markdown(&artifact.title, &artifact.content),
        ArtifactFormat::Html => artifact.content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(format: ArtifactFormat, title: &str, content: &str) -> Artifact {
        Artifact {
            id: "x".into(),
            title: title.into(),
            format,
            content: content.into(),
            created_ms: 0,
        }
    }

    #[test]
    fn markdown_renders_to_full_page_with_escaped_title() {
        let out = render(&artifact(
            ArtifactFormat::Markdown,
            "A & B <script>",
            "# Hello\n\nSome **bold** text.",
        ));
        assert!(out.starts_with("<!doctype html>"));
        // Title is escaped in the <title>.
        assert!(out.contains("<title>A &amp; B &lt;script&gt;</title>"));
        // Markdown became HTML.
        assert!(out.contains("<h1>Hello</h1>"));
        assert!(out.contains("<strong>bold</strong>"));
    }

    #[test]
    fn markdown_renders_tables() {
        let out = render_markdown("t", "| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(out.contains("<table>"), "GFM tables must render: {out}");
    }

    #[test]
    fn html_is_served_verbatim() {
        let raw = "<!doctype html><title>raw</title><p>hi</p>";
        let out = render(&artifact(ArtifactFormat::Html, "ignored", raw));
        assert_eq!(out, raw);
    }

    #[test]
    fn index_lists_links_and_escapes_titles() {
        let rows = vec![
            ArtifactMeta {
                id: "abc123".into(),
                title: "Q3 <report>".into(),
                format: ArtifactFormat::Markdown,
                created_ms: 2,
            },
            ArtifactMeta {
                id: "def456".into(),
                title: "Plain".into(),
                format: ArtifactFormat::Html,
                created_ms: 1,
            },
        ];
        let out = render_index(&rows);
        assert!(out.contains("<a href=\"/artifacts/abc123\">Q3 &lt;report&gt;</a>"));
        assert!(out.contains("<a href=\"/artifacts/def456\">Plain</a>"));
        assert!(out.contains("<title>Artifacts</title>"));
    }

    #[test]
    fn index_empty_shows_placeholder() {
        let out = render_index(&[]);
        assert!(out.contains("No artifacts published yet"));
    }

    #[test]
    fn csp_isolates_and_blocks_egress() {
        // Same-origin isolation + no-script + no-network are the properties the
        // artifact sandbox depends on. Loss of any of them is a secret-exfil path.
        assert!(ARTIFACT_CSP.contains("default-src 'none'"));
        assert!(ARTIFACT_CSP.contains("connect-src 'none'"));
        assert!(ARTIFACT_CSP.contains("script-src 'none'"));
        // `sandbox` with NO allow-tokens → opaque origin (no localStorage/cookie
        // access), no scripts, no forms, no popups, no top-navigation.
        assert!(ARTIFACT_CSP.trim_end().ends_with("sandbox"));
        assert!(!ARTIFACT_CSP.contains("allow-same-origin"));
        // Scripts must not be inline-allowed (that was the exfil path).
        assert!(!ARTIFACT_CSP.contains("script-src 'unsafe-inline'"));
    }
}
