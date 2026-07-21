//! Output styles — a Claude-Code-style response persona/verbosity that is folded
//! into the system prompt at session creation.
//!
//! A style is selected by name (`_meta.output_style`, mirroring how `rules` is
//! passed). A built-in name resolves to a curated directive; any other non-empty
//! value is treated as a **custom** style and used verbatim, so users can define
//! their own persona without a code change.

/// A built-in output style: a stable name and the directive appended to the
/// system prompt when it is selected.
pub struct BuiltinStyle {
    pub name: &'static str,
    pub summary: &'static str,
    pub directive: &'static str,
}

/// The built-in output styles. `default` is the no-op (empty directive) so
/// selecting it changes nothing.
pub const BUILTIN_STYLES: &[BuiltinStyle] = &[
    BuiltinStyle {
        name: "default",
        summary: "The standard assistant voice.",
        directive: "",
    },
    BuiltinStyle {
        name: "concise",
        summary: "Short, direct answers; minimal preamble.",
        directive: "Respond concisely. Lead with the answer or the change. Omit preamble, \
recaps, and filler. Prefer short sentences and tight code; expand only when the user asks.",
    },
    BuiltinStyle {
        name: "explanatory",
        summary: "Teaches as it works; explains the why.",
        directive: "Explain your reasoning as you go. When you make a non-obvious choice, say \
briefly why. Call out tradeoffs, edge cases, and the mental model behind the solution so the \
reader learns, not just receives.",
    },
    BuiltinStyle {
        name: "review",
        summary: "Critical, skeptical, risk-focused.",
        directive: "Adopt a critical reviewer's stance. Surface risks, edge cases, and failure \
modes before praising anything. Be specific about what could go wrong and how to verify it. \
Prefer 'here's what I'd check' over reassurance.",
    },
];

/// Look up a built-in style's directive by name (case-insensitive).
pub fn builtin_directive(name: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    BUILTIN_STYLES
        .iter()
        .find(|s| s.name == n)
        .map(|s| s.directive)
}

/// Render the `<output_style>` block to append to the system prompt for the
/// selected style `value`.
///
/// - A built-in name → its curated directive (`default` and unknown-empty → no
///   block at all).
/// - Any other non-empty value → treated as a custom directive, used verbatim.
///
/// Returns `None` when there is nothing to add (empty input, or the `default`
/// built-in whose directive is empty).
pub fn render_output_style_block(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let directive = match builtin_directive(trimmed) {
        Some(d) => d.to_string(),      // known built-in (may be empty for `default`)
        None => trimmed.to_string(),   // custom persona, used verbatim
    };
    if directive.trim().is_empty() {
        return None; // e.g. the `default` built-in
    }
    Some(format!("\n\n<output_style>\n{directive}\n</output_style>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_resolve_case_insensitively() {
        assert_eq!(builtin_directive("concise"), builtin_directive("CONCISE"));
        assert!(builtin_directive("concise").unwrap().contains("concisely"));
        assert_eq!(builtin_directive("default"), Some(""));
        assert_eq!(builtin_directive("nope"), None);
    }

    #[test]
    fn default_and_empty_produce_no_block() {
        assert_eq!(render_output_style_block(""), None);
        assert_eq!(render_output_style_block("   "), None);
        assert_eq!(render_output_style_block("default"), None);
    }

    #[test]
    fn builtin_renders_curated_directive_block() {
        let block = render_output_style_block("concise").unwrap();
        assert!(block.starts_with("\n\n<output_style>\n"));
        assert!(block.trim_end().ends_with("</output_style>"));
        assert!(block.contains("Respond concisely"));
    }

    #[test]
    fn unknown_value_is_used_as_a_custom_persona() {
        let block = render_output_style_block("Talk like a pirate, matey.").unwrap();
        assert!(block.contains("Talk like a pirate, matey."));
        assert!(block.contains("<output_style>"));
    }
}
