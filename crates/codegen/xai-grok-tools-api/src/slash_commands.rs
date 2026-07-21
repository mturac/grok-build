//! Canonical slash-command wording (`/loop`, `/imagine`, `/imagine-video`, `/goal`),
//! shared by every front-end (Grok Build shell/pager and other hosts) so
//! expansions cannot drift.

/// Canonical tool name advertised by the scheduler create tool. Gating code
/// (shell `CommandAvailability`, pager `required_tools`, host command lists)
/// keys `/loop` availability on this name.
pub const SCHEDULER_CREATE_TOOL_NAME: &str = "scheduler_create";

/// Usage hint shown when `/loop` is invoked with no arguments.
pub fn loop_usage_message() -> &'static str {
    "Usage: /loop [interval] <prompt>\n\
     Example: /loop 30m check deploy status\n\
     Example: /loop check deploy status every hour\n\n\
     Tell me how often it should run (e.g. 30m, 1 hour, every 2 days)."
}

/// Build the model instruction that `/loop` expands into for `args`.
///
/// The model, not brittle host parsing, turns the request into the
/// `scheduler_create` interval, accepting every natural phrasing and erroring
/// on bad input rather than silently defaulting. See [`loop_usage_message`].
pub fn loop_schedule_instruction(args: &str) -> String {
    format!(
        "# /loop -- schedule a recurring prompt\n\n\
         Parse the input below into an interval and a prompt, then schedule it with scheduler_create.\n\n\
         ## Deriving the interval\n\
         Read how often to run from the user's request — however they phrase it — and convert it\n\
         to a compact `<number><unit>` string, where unit is one of `s` (seconds), `m` (minutes),\n\
         `h` (hours), or `d` (days). The interval may appear at the start or end of the request;\n\
         extract it and use the remaining text as the prompt.\n\n\
         The minimum interval is 60 seconds; shorter values are raised to 60s, so tell the user if that applies.\n\n\
         If the request contains no interval at all, ask the user how often it should run before\n\
         scheduling. Do NOT invent or assume a default interval.\n\n\
         ## Action\n\
         1. Call scheduler_create with: interval (the compact string you derived), prompt,\n\
            recurring: true, fire_immediately: true. If the interval is unparseable, the tool\n\
            returns an error — fix the interval string rather than guessing.\n\
         2. Confirm: what's scheduled, the cadence, that it auto-expires after 7 days,\n\
            and that they can cancel with scheduler_delete (include the job ID).\n\
         3. Do NOT execute the prompt inline. The scheduler will fire it immediately.\n\n\
         ## Input\n\
         {args}"
    )
}

/// Canonical name of the image generation tool; gates `/imagine`.
pub const IMAGE_GEN_TOOL_NAME: &str = "image_gen";

/// Advertised name of the /imagine command.
pub const IMAGINE_COMMAND_NAME: &str = "imagine";

/// Canonical name of the image-to-video tool; gates `/imagine-video`.
pub const IMAGE_TO_VIDEO_TOOL_NAME: &str = "image_to_video";

/// Advertised name of the /imagine-video command.
pub const IMAGINE_VIDEO_COMMAND_NAME: &str = "imagine-video";

/// Usage hint shown when `/imagine` is invoked with no arguments.
pub fn imagine_usage_message() -> &'static str {
    "Usage: /imagine <description>\n\
     Provide a text description to generate an image."
}

/// Build the model instruction that `/imagine` expands into for `prompt`.
pub fn imagine_instruction(prompt: &str) -> String {
    format!(
        "Call the image_gen tool immediately, passing the user's prompt below \
         verbatim — do not rewrite, embellish, or expand it. \
         After the tool completes, briefly acknowledge and mention \
         where the image was saved.\n\n\
         Prompt: {prompt}"
    )
}

/// Usage hint shown when `/imagine-video` is invoked with no arguments.
pub fn imagine_video_usage_message() -> &'static str {
    "Usage: /imagine-video <description>\n\
     Provide a text description to generate a video."
}

/// Build the model instruction that `/imagine-video` expands into for `prompt`.
pub fn imagine_video_instruction(prompt: &str) -> String {
    format!(
        "{IMAGINE_VIDEO_SKILL}\n\n\
         User prompt: {prompt}"
    )
}

/// Video workflow guidance injected by `/imagine-video`.
const IMAGINE_VIDEO_SKILL: &str = "\
# Imagine Video

Video starts from an image — there is no text-to-video tool. \
Default to `image_to_video`; use `reference_to_video` only when the user \
explicitly asks for it or a shot genuinely needs multiple reference images.

## Default: single clip

Unless the user asks for a long video, multiple scenes, or a multi-shot sequence, \
generate **one** video:

1. Create a source image with `image_gen` that stages the first frame \
(composition, subject, lighting).
2. Call `image_to_video` with that image and a short prompt describing the motion \
or camera move (1–2 sentences, present tense).
3. After the tool completes, mention the saved file path so the user can find it.

## Longer / multi-shot videos

When the user requests a longer video, multiple scenes, or a narrative sequence:

1. **Plan the story as shots** — break the idea into distinct shots, one beat each.
2. **Favor frequent, short shots** — prefer more 6s clips over fewer long ones; more cuts keep it dynamic.
3. **Create each shot's source image** with `image_gen` (or `image_edit` to combine references), keeping characters and settings consistent across shots.
4. **Animate each shot with `image_to_video`** — the source image becomes frame 1.
5. **Assemble with FFmpeg** using stream copy (`ffmpeg -f concat ... -c copy` — never re-encode). \
Keep every shot at the same resolution and frame rate so the concat works. \
After assembly, mention the final output path.

## Shot guidance

- **Prompt-craft:** one short, vivid moment in present tense with a clear camera movement, in 1–2 sentences.
- **Minimal but interesting:** one clear subject, one simple motion or camera move per shot. Avoid complex multi-action animation; make the shot compelling through composition, lighting, and a strong moment.
- **Complex source image?** Intricate frames (busy geometry, fine detail, heavy reflections) warp when animated. Keep the subject fixed and move only the camera (slow push-in, orbit, or parallax), or break into simpler shots. For new shots, generate a simpler, animation-friendly base image rather than animating a busy one.
- **`image_to_video` animates from frame 1** — stage the first frame with `image_gen`/`image_edit` before animating.
- **Aspect ratio:** set it on the source image (`image_gen` `aspect_ratio`); don't re-crop an existing video.
- **Duration:** 6s or 10s only (prefer 6s); round to the nearest.
- **Real people:** reference-first — drive the video from a verified reference image; never animate a named person without one.
- Don't loop the same clip unless asked.";

// ── /review and /security-review ────────────────────────────────

/// Advertised name of the /review command.
pub const REVIEW_COMMAND_NAME: &str = "review";

/// Advertised name of the /security-review command.
pub const SECURITY_REVIEW_COMMAND_NAME: &str = "security-review";

/// Usage hint shown when `/review` is invoked with no arguments.
pub fn review_usage_message() -> &'static str {
    "Usage: /review [target]\n\
     Examples:\n\
     /review                 review the uncommitted + staged changes\n\
     /review main..HEAD      review a commit range\n\
     /review src/auth.rs     review a path\n\n\
     Runs a multi-dimension code review and reports ranked, verified findings. \
     Does not change code."
}

/// Shared scope paragraph for the review workflows: how to resolve `target`
/// into a concrete diff without guessing.
const REVIEW_SCOPE: &str = "\
## Scope\n\
The target below says WHAT to review. Resolve it into a concrete change set:\n\
- empty → the working changes: `git diff HEAD` (unstaged + staged). If that is \
empty, say so and stop.\n\
- a commit range (`main..HEAD`, `abc123..def456`) → `git diff <range>`.\n\
- a path or glob → review that path's current contents (and its diff if it has \
uncommitted changes).\n\
- a PR number / URL → fetch the PR diff (e.g. `gh pr diff <n>`); if unavailable, \
say so and ask.\n\
Read enough surrounding code to judge each change in context — a diff alone \
hides callers, invariants, and error paths.";

/// Shared verify+report+boundaries footer for the review workflows.
const REVIEW_CONTRACT: &str = "\
## Verify before reporting\n\
Every candidate finding must be checked against the actual code before it \
reaches the report. State a concrete failure path (inputs/state → wrong \
outcome); if you cannot, drop it. Prefer a short, high-signal list over a long \
speculative one — a false positive costs the reader more than a missed nitpick.\n\n\
## Report\n\
Rank findings most-severe first. For each: a one-line summary, the exact \
`file:line`, the concrete failure scenario, and a suggested fix. If nothing \
substantive turns up, say so plainly rather than padding.\n\n\
## Boundaries\n\
Review only — do NOT edit, stage, commit, push, or merge anything. If the user \
wants fixes, they will ask in a follow-up.";

/// Build the model instruction that `/review` expands into for `args`.
///
/// Drives a multi-dimension review that fans out with the `orchestrate` tool
/// when it is available and falls back to a direct review otherwise. The
/// wording lives here so every front-end expands `/review` identically.
pub fn review_instruction(args: &str) -> String {
    let target = args.trim();
    let target_line = if target.is_empty() {
        "(none given — review the working changes)".to_string()
    } else {
        target.to_string()
    };
    format!(
        "# /review -- multi-dimension code review\n\n\
         Review the target below and report ranked, verified findings.\n\n\
         {REVIEW_SCOPE}\n\n\
         ## Dimensions\n\
         Cover these lenses; each is a distinct failure mode, not a restatement:\n\
         - Correctness — logic errors, off-by-one, wrong conditionals, unhandled \
         cases, race conditions, resource leaks.\n\
         - Error handling — swallowed errors, panics/unwraps on untrusted input, \
         missing validation at boundaries.\n\
         - Tests — missing coverage for new/changed behavior, tests that assert \
         nothing, happy-path-only suites.\n\
         - Security — injection, authz gaps, secret exposure (see /security-review \
         for a dedicated pass).\n\
         - Maintainability — duplicated logic, dead code, misleading names, \
         needless complexity.\n\n\
         ## Method\n\
         If the `orchestrate` tool is available, fan out one reviewer per \
         dimension in parallel, then adversarially verify each returned finding \
         with a skeptical second pass before accepting it. If it is not \
         available, do the same passes yourself, sequentially. Either way, the \
         verify step is mandatory.\n\n\
         {REVIEW_CONTRACT}\n\n\
         ## Target\n\
         {target_line}"
    )
}

/// Usage hint shown when `/security-review` is invoked with no arguments.
pub fn security_review_usage_message() -> &'static str {
    "Usage: /security-review [target]\n\
     Examples:\n\
     /security-review              audit the uncommitted + staged changes\n\
     /security-review main..HEAD   audit a commit range\n\
     /security-review src/api      audit a path\n\n\
     Runs a security-focused audit and reports ranked, verified \
     vulnerabilities. Does not change code."
}

/// Build the model instruction that `/security-review` expands into for `args`.
///
/// A security-focused variant of [`review_instruction`] with an
/// exploit-oriented rubric and severity ratings.
pub fn security_review_instruction(args: &str) -> String {
    let target = args.trim();
    let target_line = if target.is_empty() {
        "(none given — audit the working changes)".to_string()
    } else {
        target.to_string()
    };
    format!(
        "# /security-review -- security audit\n\n\
         Audit the target below for security vulnerabilities and report ranked, \
         verified findings. Think like an attacker: assume all external input is \
         hostile.\n\n\
         {REVIEW_SCOPE}\n\n\
         ## Threat lenses\n\
         - Injection — SQL/NoSQL, OS command, path traversal, template, log \
         injection.\n\
         - AuthN/AuthZ — missing or bypassable authentication; authorization not \
         enforced at the resource level; trusting client-supplied identity.\n\
         - Secrets — hardcoded credentials/keys/tokens; secrets in logs, errors, \
         or responses.\n\
         - Input validation — unvalidated size/type/range; SSRF; unsafe \
         deserialization; XXE.\n\
         - Web — XSS (stored/reflected/DOM), CSRF, open redirect, insecure CORS, \
         missing security headers.\n\
         - Crypto — weak or home-rolled algorithms, static IVs/salts, predictable \
         randomness, missing verification.\n\
         - Memory/runtime (esp. Rust) — `unsafe` blocks, integer overflow, \
         panics reachable from untrusted input (DoS), TOCTOU.\n\
         - Supply chain — unpinned or abandoned dependencies, known-vulnerable \
         versions.\n\n\
         ## Method\n\
         If the `orchestrate` tool is available, fan out the lenses in parallel, \
         then adversarially verify each candidate — construct the concrete \
         exploit path — before accepting it. If it is not available, do the same \
         passes yourself. The verify step is mandatory: a security false positive \
         erodes trust in the whole report.\n\n\
         ## Report\n\
         Rank by severity (Critical / High / Medium / Low). For each: a one-line \
         summary, the exact `file:line`, a concrete exploit scenario (how an \
         attacker reaches and abuses it), and a specific remediation. If the code \
         is clean for a lens, say so briefly.\n\n\
         ## Boundaries\n\
         Audit only — do NOT edit, stage, commit, push, or merge anything. Do not \
         run exploit code against live systems. If the user wants fixes, they will \
         ask in a follow-up.\n\n\
         ## Target\n\
         {target_line}"
    )
}

pub const UPDATE_GOAL_TOOL_NAME: &str = "update_goal";

pub const GOAL_COMMAND_NAME: &str = "goal";

/// Bare subcommand tokens reserved for goal lifecycle control rather than
/// being treated as an objective, matching the shell's /goal grammar.
pub const GOAL_RESERVED_SUBCOMMANDS: &[&str] = &["status", "pause", "resume", "clear", "edit"];

pub fn goal_usage_message() -> &'static str {
    "Usage: /goal <objective>\n\
     Set an objective to work toward until it is complete."
}

pub fn goal_instruction(objective: &str) -> String {
    format!(
        "# /goal -- pursue an objective\n\n\
         A goal has been set: {objective}\n\n\
         Work directly on this goal and carry it as far as you can. Deliver \
         everything the user asked for yourself: no follow-up questions, no \
         manual steps left for the user. If the conversation continues, keep \
         pursuing the goal until it is complete.\n\n\
         TRACKING: break the objective into concrete steps and track them \
         (use your todo tool if one is available), marking each done as you \
         finish it.\n\n\
         VERIFY AS YOU GO: test each change on the real path before moving on. \
         A completion claim must be backed by evidence produced in this \
         session, not assumptions.\n\n\
         Call update_goal(completed: true, message: \"summary\") ONLY when the \
         goal is fully achieved. Call update_goal(blocked_reason: \"reason\") \
         only when truly stuck after 3+ consecutive failed attempts at the \
         same problem. Call update_goal(message: \"status note\") to log \
         progress along the way. If update_goal returns an error, continue \
         working the goal and report status in your reply instead.\n\n\
         Start now."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imagine_instruction_carries_prompt_verbatim() {
        let text = imagine_instruction("a golden sunset");
        assert!(text.contains("a golden sunset"));
        assert!(text.contains("image_gen"));
        assert!(text.contains("verbatim"));
    }

    #[test]
    fn imagine_video_instruction_carries_prompt_and_workflow() {
        let text = imagine_video_instruction("a cat playing piano");
        assert!(text.contains("a cat playing piano"));
        assert!(text.contains("image_to_video"));
        assert!(text.contains("FFmpeg"));
    }

    #[test]
    fn instruction_carries_args_and_contract_tokens() {
        let text = loop_schedule_instruction("every 30 minutes do x");
        assert!(text.contains("every 30 minutes do x"));
        assert!(text.contains("<number><unit>"));
        assert!(text.contains("ask the user how often"));
        assert!(!text.contains("10m"), "no host-side default interval");
    }

    #[test]
    fn goal_instruction_carries_objective_and_contract_tokens() {
        let text = goal_instruction("ship the widget");
        assert!(text.contains("ship the widget"));
        assert!(text.contains("update_goal(completed: true"));
        assert!(text.contains("blocked_reason"));
        assert!(text.contains("If update_goal returns an error"));
        assert!(
            !text.contains("system-reminder"),
            "expansions ride as user messages and must not claim reminder authority"
        );
        assert!(goal_usage_message().contains("Usage: /goal"));
    }

    #[test]
    fn usage_message_has_no_default_claim() {
        assert!(loop_usage_message().contains("Usage: /loop"));
        assert!(!loop_usage_message().contains("10m"));
    }

    #[test]
    fn review_instruction_carries_target_and_contract_tokens() {
        let text = review_instruction("main..HEAD");
        assert!(text.contains("main..HEAD"), "target must appear: {text}");
        // Uses orchestrate opportunistically, not as a hard requirement.
        assert!(text.contains("orchestrate"));
        assert!(text.contains("If it is not available"));
        // Verify step and read-only boundary are the load-bearing contract.
        assert!(text.contains("Verify before reporting"));
        assert!(text.contains("do NOT edit, stage, commit, push, or merge"));
        // Rides as a user message; must not claim system-reminder authority.
        assert!(!text.contains("system-reminder"));
    }

    #[test]
    fn review_instruction_empty_target_reviews_working_changes() {
        let text = review_instruction("   ");
        assert!(text.contains("review the working changes"));
        assert!(review_usage_message().contains("Usage: /review"));
    }

    #[test]
    fn security_review_instruction_carries_target_and_rubric() {
        let text = security_review_instruction("src/api");
        assert!(text.contains("src/api"), "target must appear: {text}");
        assert!(text.contains("Injection"));
        assert!(text.contains("exploit scenario"));
        // Severity ladder present.
        assert!(text.contains("Critical / High / Medium / Low"));
        // Read-only + no live exploitation.
        assert!(text.contains("do NOT edit, stage, commit, push, or merge"));
        assert!(text.contains("Do not run exploit code"));
        assert!(!text.contains("system-reminder"));
    }

    #[test]
    fn security_review_empty_target_audits_working_changes() {
        let text = security_review_instruction("");
        assert!(text.contains("audit the working changes"));
        assert!(security_review_usage_message().contains("Usage: /security-review"));
    }
}
