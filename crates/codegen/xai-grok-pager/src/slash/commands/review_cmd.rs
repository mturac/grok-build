//! `/review` and `/security-review` — curated review workflows.
//!
//! Both expand into a curated instruction (shared wording from
//! `xai-grok-tools`, so shell↔pager parity holds) that drives a
//! multi-dimension review, fanning out with the `orchestrate` tool when it is
//! available and reviewing directly otherwise. Unlike `/loop`, an empty
//! argument is valid — it reviews the working changes — so there is no
//! usage-only path; the instruction resolves an empty target itself.

use agent_client_protocol as acp;
use xai_grok_tools::implementations::grok_build::{
    review_instruction, security_review_instruction,
};

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

const REVIEW_ARG_HINT: &str = "[target: range | path | PR — default: working changes]";

pub struct ReviewCommand;

impl SlashCommand for ReviewCommand {
    fn name(&self) -> &str {
        "review"
    }

    fn description(&self) -> &str {
        "Multi-dimension code review of a target (verified findings)"
    }

    fn usage(&self) -> &str {
        "/review [target]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some(REVIEW_ARG_HINT)
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        CommandResult::InjectSkill {
            display_text: display_text("review", args),
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                review_instruction(args),
            ))],
            display_as_skill: false,
            scheduled_task_preview: None,
        }
    }
}

pub struct SecurityReviewCommand;

impl SlashCommand for SecurityReviewCommand {
    fn name(&self) -> &str {
        "security-review"
    }

    fn description(&self) -> &str {
        "Security audit of a target (ranked, verified vulnerabilities)"
    }

    fn usage(&self) -> &str {
        "/security-review [target]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some(REVIEW_ARG_HINT)
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        CommandResult::InjectSkill {
            display_text: display_text("security-review", args),
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                security_review_instruction(args),
            ))],
            display_as_skill: false,
            scheduled_task_preview: None,
        }
    }
}

/// Compact invocation string for the timeline / replay: `/name` or `/name args`.
fn display_text(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {args}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;

    fn ctx_and_run(cmd: &dyn SlashCommand, args: &str) -> CommandResult {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        cmd.run(&mut ctx, args)
    }

    #[test]
    fn review_injects_shared_instruction_and_display_text() {
        match ctx_and_run(&ReviewCommand, "main..HEAD") {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                display_as_skill,
                scheduled_task_preview,
            } => {
                assert_eq!(display_text, "/review main..HEAD");
                assert!(!display_as_skill);
                assert!(scheduled_task_preview.is_none());
                let acp::ContentBlock::Text(tb) = &prompt_blocks[0] else {
                    panic!("expected text block");
                };
                // Drift guard: pager text == shared helper.
                assert_eq!(tb.text, review_instruction("main..HEAD"));
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
    }

    #[test]
    fn review_empty_args_is_valid_and_uses_bare_display_text() {
        match ctx_and_run(&ReviewCommand, "   ") {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/review");
                let acp::ContentBlock::Text(tb) = &prompt_blocks[0] else {
                    panic!("expected text block");
                };
                assert_eq!(tb.text, review_instruction("   "));
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
    }

    #[test]
    fn security_review_injects_shared_instruction() {
        match ctx_and_run(&SecurityReviewCommand, "src/api") {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/security-review src/api");
                let acp::ContentBlock::Text(tb) = &prompt_blocks[0] else {
                    panic!("expected text block");
                };
                assert_eq!(tb.text, security_review_instruction("src/api"));
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
    }
}
