//! New-architecture tool implementations (NewTool trait).
//!
//! Each sub-module here contains a tool that implements `NewTool` instead
//! of the old `Tool` trait. During migration, old implementations live in
//! `implementations/<tool>/` and new implementations live in
//! `implementations/grok_build/<tool>/`.
//!
//! The [`register_all()`] function is the single entry-point for wiring up
//! the standard toolset. It inserts shared resources (`Terminal`,
//! `AvailableSkills`, `BashParams`) and registers every built-in tool.
pub mod artifact;
pub mod ask_user_question;
pub mod bash;
pub mod ci_guard;
pub mod code_context;
pub mod orchestrate;
#[path = "deploy_app_stub.rs"]
pub mod deploy_app;
pub mod enter_plan_mode;
pub mod exit_plan_mode;
pub mod grep;
pub mod image_edit;
pub mod image_gen;
pub mod kill_task;
pub mod list_dir;
pub mod lsp;
pub mod monitor;
pub mod read_file;
pub mod scheduler;
pub mod search_replace;
pub(crate) mod storage;
pub mod task;
pub mod task_output;
pub mod todo;
pub mod update_goal;
pub mod video_gen;
pub mod web_fetch;
pub mod web_search;
pub use artifact::{
    ARTIFACT_TOOL_NAME, ArtifactService, ArtifactStore, ArtifactTool, artifact_service,
    set_artifact_service,
};
pub use ask_user_question::AskUserQuestionTool;
pub use bash::BashTool;
pub use code_context::{CODE_CONTEXT_TOOL_NAME, CodeContextTool};
pub use deploy_app::{AppBuilderDeployerConfig, DEPLOY_APP_TOOL_NAME};
pub use enter_plan_mode::EnterPlanModeTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use grep::GrepTool;
pub use image_edit::{IMAGE_EDIT_TOOL_NAME, ImageEditTool};
pub use image_gen::{
    IMAGE_GEN_TOOL_NAME, IMAGINE_COMMAND_NAME, ImageGenTool, imagine_instruction,
    imagine_usage_message,
};
pub use kill_task::{KillTaskTool, KillTerminalCommandTool};
pub use list_dir::ListDirTool;
pub use lsp::LspTool;
pub use ci_guard::tool::{CI_GUARD_TOOL_NAME, CiGuardTool};
pub use orchestrate::{ORCHESTRATE_TOOL_NAME, OrchestrateTool};
pub use monitor::tool::MonitorTool;
pub use read_file::ReadFileTool;
pub use scheduler::create::{
    SCHEDULER_CREATE_TOOL_NAME, SchedulerCreateTool, loop_schedule_instruction, loop_usage_message,
};
pub use scheduler::delete::{SCHEDULER_DELETE_TOOL_NAME, SchedulerDeleteTool};
pub use scheduler::list::SchedulerListTool;
pub use search_replace::SearchReplaceTool;
pub use task::TaskTool;
pub use task_output::{GetTerminalCommandOutputTool, TaskOutputTool, WaitTasksTool};
pub use todo::TodoWriteTool;
pub use update_goal::{UPDATE_GOAL_TOOL_NAME, UpdateGoalTool};
pub use video_gen::{
    IMAGE_TO_VIDEO_TOOL_NAME, IMAGINE_VIDEO_COMMAND_NAME, ImageToVideoTool,
    REFERENCE_TO_VIDEO_TOOL_NAME, ReferenceToVideoTool, imagine_video_instruction,
    imagine_video_usage_message,
};
pub use web_fetch::{WebFetchClient, WebFetchConfig, WebFetchParams, WebFetchTool};
pub use web_search::WebSearchTool;

// Curated review workflows (`/review`, `/security-review`). Wording lives in
// the shared `slash_commands` module so every front-end expands identically,
// re-exported here alongside `loop_*` so shell/pager import from one path.
pub use xai_grok_tools_api::slash_commands::{
    REVIEW_COMMAND_NAME, SECURITY_REVIEW_COMMAND_NAME, review_instruction, review_usage_message,
    security_review_instruction, security_review_usage_message,
};

#[cfg(test)]
mod review_command_drift_tests {
    use super::orchestrate::ORCHESTRATE_TOOL_NAME;
    use super::{review_instruction, security_review_instruction};

    /// The review workflows name the orchestrate tool in prose. Pin that prose
    /// to the real advertised tool name so a rename of the tool surfaces here
    /// instead of leaving the instruction pointing at a tool that no longer
    /// exists.
    #[test]
    fn review_prose_matches_real_orchestrate_tool_name() {
        assert!(review_instruction("x").contains(ORCHESTRATE_TOOL_NAME));
        assert!(security_review_instruction("x").contains(ORCHESTRATE_TOOL_NAME));
    }
}
