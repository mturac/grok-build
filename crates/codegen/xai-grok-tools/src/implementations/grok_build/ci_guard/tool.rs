//! The `ci_guard` agent tool — the Option-A command surface for the CI Guardian.
//!
//! One tool with an `action`. `start` registers a durable scheduler task whose
//! fired prompt tells the agent to call `action=check`; `check` gates on
//! preflight/probe/dedup/budget and, on a confident code failure, hands the
//! agent the logs + diagnosis and asks it to produce edits and call
//! `action=commit_fix`; `commit_fix` enforces the branch/never-push/budget
//! guards via [`super::fix::apply_fix`]. `stop`/`status`/`rearm` manage watches.
//!
//! State lives in the durable `CiGuardState` resource, auto-persisted by the
//! toolset after each call. Idempotency across a crash is guaranteed by the
//! deterministic fix-branch name (a re-attempt hits `git checkout -b` on an
//! existing branch and errors rather than duplicating a commit).

use tokio::sync::oneshot;

use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::State;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{resolve_cwd, shared_resources};

use super::classify::{FailureClass, classify};
use super::fix::{self, FileEdit, FixOutcome};
use super::gh;
use super::types::{CiGuardState, CiWatch};

use crate::implementations::grok_build::scheduler::types::{
    ScheduledTask, SchedulerCommand, SchedulerHandle,
};

pub const CI_GUARD_TOOL_NAME: &str = "ci_guard";

/// How often a watch polls the PR's CI.
const WATCH_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CiGuardAction {
    /// Begin watching a PR's CI (creates a recurring durable poll task).
    Start,
    /// Stop watching a PR (deletes its poll task).
    Stop,
    /// List active watches and their remaining automation budget.
    Status,
    /// Grant one more autonomous fix attempt for a PR.
    Rearm,
    /// One poll tick: check CI state; on a new confident failure, return the
    /// diagnosis so the agent can produce a fix.
    Check,
    /// Apply agent-produced edits as a fix on an isolated local branch (never
    /// pushes); consumes the PR's automation budget.
    CommitFix,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CiFileEditInput {
    /// Path relative to the repo root.
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CiGuardInput {
    pub action: CiGuardAction,
    /// `owner/name`. Required for start/stop/rearm/check/commit_fix.
    #[serde(default)]
    pub repo: Option<String>,
    /// PR number. Required for start/stop/rearm/check/commit_fix.
    #[serde(default)]
    pub pr: Option<u64>,
    /// For commit_fix: the head SHA the fix was diagnosed against (from check).
    #[serde(default)]
    pub head_sha: Option<String>,
    /// For commit_fix: the file edits to apply.
    #[serde(default)]
    pub files: Option<Vec<CiFileEditInput>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CiGuardOutput {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

impl xai_tool_runtime::ToolOutput for CiGuardOutput {}

fn ok(message: impl Into<String>, data: serde_json::Value) -> CiGuardOutput {
    CiGuardOutput {
        ok: true,
        message: message.into(),
        data,
    }
}
fn not_ok(message: impl Into<String>) -> CiGuardOutput {
    CiGuardOutput {
        ok: false,
        message: message.into(),
        data: serde_json::Value::Null,
    }
}

fn require<'a>(
    v: &'a Option<String>,
    what: &str,
) -> Result<&'a str, xai_tool_runtime::ToolError> {
    v.as_deref()
        .ok_or_else(|| xai_tool_runtime::ToolError::invalid_arguments(format!("`{what}` is required")))
}
fn require_pr(v: Option<u64>) -> Result<u64, xai_tool_runtime::ToolError> {
    v.ok_or_else(|| xai_tool_runtime::ToolError::invalid_arguments("`pr` is required"))
}

#[derive(Debug, Default)]
pub struct CiGuardTool;

impl crate::types::tool_metadata::ToolMetadata for CiGuardTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        r#"Watch a GitHub PR's CI and, on a confident code failure, prepare a fix on a local branch (never pushed) for you to review.

Actions:
- start {repo, pr}: begin watching (polls every 5 min). Requires `gh` auth.
- check {repo, pr}: one poll tick (the watch's fired prompt calls this). On a new confident code failure it returns the logs + diagnosis; then produce the file edits and call commit_fix.
- commit_fix {repo, pr, head_sha, files}: apply your edits on branch grok-ci-fix/<pr>-<sha> and commit locally. NEVER pushes. Consumes the PR's one-attempt budget.
- rearm {repo, pr}: allow one more autonomous fix after you've reviewed the last one.
- stop {repo, pr} / status.

Safety: one autonomous fix attempt per PR until you rearm; a new failing commit does NOT refill it. The agent never pushes, merges, or deploys."#
    }
    fn emitted_notifications(&self) -> &'static [&'static str] {
        &["ScheduledTaskCreated"]
    }
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl CiGuardTool {
    fn scheduler_handle(
        res: &crate::types::resources::Resources,
    ) -> Result<tokio::sync::mpsc::UnboundedSender<SchedulerCommand>, xai_tool_runtime::ToolError>
    {
        Ok(res
            .get::<SchedulerHandle>()
            .ok_or_else(|| xai_tool_runtime::ToolError::custom("missing_resource", "SchedulerHandle"))?
            .0
            .clone())
    }
}

impl xai_tool_runtime::Tool for CiGuardTool {
    type Args = CiGuardInput;
    type Output = CiGuardOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(CI_GUARD_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            CI_GUARD_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.ci_guard", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: CiGuardInput,
    ) -> Result<CiGuardOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;

        match input.action {
            CiGuardAction::Status => {
                let res = resources.lock().await;
                let watches = res
                    .get::<State<CiGuardState>>()
                    .map(|s| s.watches.clone())
                    .unwrap_or_default();
                let active: Vec<_> = watches.iter().filter(|w| w.watch_active).collect();
                let data = serde_json::to_value(&active).unwrap_or(serde_json::Value::Null);
                Ok(ok(format!("{} active watch(es).", active.len()), data))
            }

            CiGuardAction::Start => {
                let repo = require(&input.repo, "repo")?.to_string();
                let pr = require_pr(input.pr)?;

                if let gh::Preflight::Blocked(reason) = gh::preflight(&repo).await {
                    return Ok(not_ok(format!(
                        "CI Guardian preflight blocked ({reason}); fix `gh` auth/access and retry."
                    )));
                }

                let prompt = format!(
                    "CI guardian tick for {repo} #{pr}: call the ci_guard tool with action=\"check\", \
                     repo=\"{repo}\", pr={pr}. If it reports a confident code failure, read the logs, \
                     produce the minimal file edits to fix it, and call ci_guard action=\"commit_fix\" \
                     with repo, pr, the reported headSha, and your files. Never push."
                );
                let task = ScheduledTask::new(WATCH_INTERVAL_SECS, prompt, true, true);
                let sender = {
                    let res = resources.lock().await;
                    Self::scheduler_handle(&res)?
                };
                let (reply_tx, reply_rx) = oneshot::channel();
                sender
                    .send(SchedulerCommand::Create {
                        task: task.clone(),
                        reply: reply_tx,
                    })
                    .map_err(|_| {
                        xai_tool_runtime::ToolError::custom("process_manager", "Scheduler stopped")
                    })?;
                let created = reply_rx
                    .await
                    .map_err(|_| {
                        xai_tool_runtime::ToolError::custom("process_manager", "Scheduler dropped reply")
                    })?
                    .map_err(|e| xai_tool_runtime::ToolError::invalid_arguments(e.to_string()))?;

                {
                    let mut res = resources.lock().await;
                    let st = res.get_or_default::<State<CiGuardState>>();
                    st.watches
                        .retain(|w| !(w.pr_number == pr && w.repo == repo));
                    st.watches
                        .push(CiWatch::new(pr, repo.clone(), created.id.clone()));
                }
                Ok(ok(
                    format!(
                        "Watching {repo} #{pr} every 5 minutes. On a CI failure I'll diagnose it and, \
                         if it's a confident code bug, prepare a fix on a local branch for you to review \
                         and push — I never push myself."
                    ),
                    serde_json::json!({ "schedulerTaskId": created.id }),
                ))
            }

            CiGuardAction::Stop => {
                let repo = require(&input.repo, "repo")?.to_string();
                let pr = require_pr(input.pr)?;
                let task_id = {
                    let res = resources.lock().await;
                    res.get::<State<CiGuardState>>()
                        .and_then(|s| s.watch(&repo, pr))
                        .map(|w| w.scheduler_task_id.clone())
                };
                if let Some(tid) = task_id {
                    let sender = {
                        let res = resources.lock().await;
                        Self::scheduler_handle(&res)?
                    };
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let _ = sender.send(SchedulerCommand::Delete {
                        id: tid,
                        reply: reply_tx,
                    });
                    let _ = reply_rx.await;
                }
                {
                    let mut res = resources.lock().await;
                    if let Some(w) = res.get_or_default::<State<CiGuardState>>().watch_mut(&repo, pr) {
                        w.watch_active = false;
                    }
                }
                Ok(ok(format!("Stopped watching {repo} #{pr}."), serde_json::Value::Null))
            }

            CiGuardAction::Rearm => {
                let repo = require(&input.repo, "repo")?.to_string();
                let pr = require_pr(input.pr)?;
                let mut res = resources.lock().await;
                match res.get_or_default::<State<CiGuardState>>().watch_mut(&repo, pr) {
                    Some(w) => {
                        w.rearm();
                        Ok(ok(
                            format!("Re-armed {repo} #{pr}: one more autonomous fix attempt allowed."),
                            serde_json::Value::Null,
                        ))
                    }
                    None => Ok(not_ok(format!("No watch for {repo} #{pr}."))),
                }
            }

            CiGuardAction::Check => {
                let repo = require(&input.repo, "repo")?.to_string();
                let pr = require_pr(input.pr)?;

                if let gh::Preflight::Blocked(reason) = gh::preflight(&repo).await {
                    return Ok(not_ok(format!("CI Guardian blocked: {reason}")));
                }
                let status = match gh::probe_pr(&repo, pr).await {
                    Ok(s) => s,
                    Err(e) => return Ok(not_ok(format!("CI probe failed: {e}"))),
                };

                match status.state {
                    gh::CiState::MergedOrClosed => {
                        // Auto-cleanup: delete the poll task and deactivate.
                        let task_id = {
                            let res = resources.lock().await;
                            res.get::<State<CiGuardState>>()
                                .and_then(|s| s.watch(&repo, pr))
                                .map(|w| w.scheduler_task_id.clone())
                        };
                        if let Some(tid) = task_id {
                            let sender = {
                                let res = resources.lock().await;
                                Self::scheduler_handle(&res)?
                            };
                            let (tx, rx) = oneshot::channel();
                            let _ = sender.send(SchedulerCommand::Delete { id: tid, reply: tx });
                            let _ = rx.await;
                        }
                        {
                            let mut res = resources.lock().await;
                            if let Some(w) =
                                res.get_or_default::<State<CiGuardState>>().watch_mut(&repo, pr)
                            {
                                w.watch_active = false;
                            }
                        }
                        return Ok(ok("PR merged/closed; watch stopped.", serde_json::Value::Null));
                    }
                    gh::CiState::Passed => return Ok(ok("CI passing; nothing to do.", serde_json::Value::Null)),
                    gh::CiState::Pending => return Ok(ok("CI still running.", serde_json::Value::Null)),
                    gh::CiState::Failed => {}
                }

                // New failure? Dedup on (head_sha, failing_check); record either way.
                let (is_new, budget) = {
                    let res = resources.lock().await;
                    match res.get::<State<CiGuardState>>().and_then(|s| s.watch(&repo, pr)) {
                        Some(w) => {
                            let is_new = w.last_head_sha.as_deref() != Some(status.head_sha.as_str())
                                || w.last_failing_check.as_deref() != status.failing_check.as_deref();
                            (is_new, w.budget_available())
                        }
                        None => (true, true), // no watch record yet — treat as new
                    }
                };
                if !is_new {
                    return Ok(ok("Already handled this failure.", serde_json::Value::Null));
                }
                {
                    let mut res = resources.lock().await;
                    if let Some(w) = res.get_or_default::<State<CiGuardState>>().watch_mut(&repo, pr) {
                        w.last_head_sha = Some(status.head_sha.clone());
                        w.last_failing_check = status.failing_check.clone();
                    }
                }

                let logs = gh::fetch_failure_logs(&repo, &status.head_sha).await;
                let class = classify(&logs, &[]);
                let logs_tail: String = logs.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");

                match class {
                    FailureClass::ConfidentCode { failing_tests } if budget => Ok(ok(
                        "Confident code failure. Produce the minimal fix edits and call ci_guard \
                         action=commit_fix with repo, pr, this headSha, and your files.",
                        serde_json::json!({
                            "state": "failed",
                            "diagnosis": "confident_code",
                            "failingTests": failing_tests,
                            "headSha": status.head_sha,
                            "logsTail": logs_tail,
                        }),
                    )),
                    FailureClass::ConfidentCode { .. } => Ok(ok(
                        "Confident code failure, but the automation budget is spent. Run \
                         ci_guard action=rearm to allow another fix.",
                        serde_json::json!({ "state": "failed", "budget": "spent" }),
                    )),
                    other => Ok(ok(
                        format!("CI failed but not auto-fixable (diagnosis: {other:?})."),
                        serde_json::json!({
                            "state": "failed",
                            "diagnosis": format!("{other:?}"),
                            "logsTail": logs_tail,
                        }),
                    )),
                }
            }

            CiGuardAction::CommitFix => {
                let repo = require(&input.repo, "repo")?.to_string();
                let pr = require_pr(input.pr)?;
                let head_sha = require(&input.head_sha, "head_sha")?.to_string();
                let files = input.files.unwrap_or_default();
                if files.is_empty() {
                    return Ok(not_ok("`files` is required and must be non-empty for commit_fix."));
                }

                // Gate on budget but DO NOT consume yet — a failed/refused fix
                // must not burn the one-attempt token. Consume only after
                // apply_fix reports Prepared (below). Combined with the
                // deterministic branch name (a crash-retry hits an existing
                // branch and errors), this keeps the token accounting correct.
                let has_budget = {
                    let res = resources.lock().await;
                    res.get::<State<CiGuardState>>()
                        .and_then(|s| s.watch(&repo, pr))
                        .map(|w| w.budget_available())
                        .unwrap_or(false)
                };
                if !has_budget {
                    return Ok(not_ok(
                        "No automation budget for this PR (already used, or no active watch). \
                         Run ci_guard action=start, then action=rearm if a prior fix was used.",
                    ));
                }

                let cwd = resolve_cwd(&ctx, &resources).await?;
                let patch: Vec<FileEdit> = files
                    .into_iter()
                    .map(|f| FileEdit {
                        path: f.path,
                        contents: f.contents,
                    })
                    .collect();
                let outcome = fix::apply_fix(
                    &cwd,
                    pr,
                    &head_sha,
                    &patch,
                    &format!("ci: prepare fix for PR #{pr}"),
                )
                .await
                .map_err(|e| xai_tool_runtime::ToolError::custom("ci_guard_fix", e.to_string()))?;

                match outcome {
                    FixOutcome::Prepared {
                        branch,
                        diff_summary,
                    } => {
                        // Fix succeeded — NOW spend the one-attempt budget.
                        {
                            let mut res = resources.lock().await;
                            if let Some(w) = res
                                .get_or_default::<State<CiGuardState>>()
                                .watch_mut(&repo, pr)
                            {
                                w.consume_attempt();
                            }
                        }
                        Ok(ok(
                            format!(
                                "Fix prepared on branch {branch}. Review and push it yourself — CI Guardian never pushes."
                            ),
                            serde_json::json!({ "branch": branch, "diffSummary": diff_summary }),
                        ))
                    }
                    FixOutcome::RefusedDirtyWorktree => {
                        Ok(not_ok("Refused: the worktree has uncommitted changes. Commit or stash them first."))
                    }
                    FixOutcome::StaleHeadAborted => {
                        Ok(not_ok("Aborted: the PR head moved since diagnosis. Run ci_guard action=check again."))
                    }
                    FixOutcome::NoFix => Ok(not_ok("No file edits were applied.")),
                }
            }
        }
    }
}
