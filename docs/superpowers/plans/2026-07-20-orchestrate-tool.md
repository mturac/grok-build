# `orchestrate` Tool — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a deterministic `orchestrate` agent tool that fans N sub-tasks out concurrently over the existing `SubagentBackend`, barriers on all, and gathers via an optional synthesize/verify step.

**Architecture:** A thin, deterministic layer over `SubagentBackend::spawn` (injected as `SubagentBackendResource`, the same seam `Task` uses). Pure input validation + a stub-testable fan-out/gather core, then the tool + registry wiring. No new runtime; the coordinator's concurrency caps apply.

**Tech Stack:** Rust, `xai-grok-tools` (fast crate), `futures::future::join_all` for the barrier, `async_trait`, the existing `xai_tool_runtime::Tool` framework.

## Global Constraints
- Commit identity `mturac <345446+mturac@users.noreply.github.com>`; NO AI/Claude attribution in messages.
- Root `Cargo.toml` read-only; no new deps expected (`futures` already used across the crate).
- `CARGO_INCREMENTAL=0`. `xai-grok-tools` compiles fast; the shell only needs a build check at the end (catch-all match arms mean no shell code change).
- Sub-agents inherit the parent's capabilities; `orchestrate` grants nothing new. `run_in_background:false`, `surface_completion:false` for all spawned sub-tasks.
- `MAX_SUBTASKS = 16`, `MAX_ORCHESTRATE_DEPTH = 2`.

## Grounding to confirm first (real code, not fabricated)
- `SubagentBackend` trait + `SubagentBackendResource` — `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/backend.rs` (`async fn spawn(&self, SubagentRequest) -> Result<SubagentResult, ToolError>`).
- `SubagentRequest` / `SubagentResult` fields + the exact request construction — `.../grok_build/task/types.rs` and the request built in `.../grok_build/task/mod.rs:294-321` (mirror its field defaults; `SubagentResult { success, output: Arc<str>, error, cancelled, subagent_id, ... }`).
- Resources read by `Task`: `SubagentBackendResource`, `SessionIdResource`, `CurrentPromptIdResource`, `SubagentDepthCounter` (`task/mod.rs:124-144`).
- Tool trait + registration pattern — mirror the just-merged `ci_guard` tool (`ci_guard/tool.rs`, `registry/types.rs`, `types/tool_io.rs`, `types/output.rs`, `normalization.rs`, `reminders/task_completion.rs`).

---

### Task 1: types + argument validation

**Files:**
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/orchestrate/mod.rs`
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/orchestrate/types.rs`
- Modify: `.../grok_build/mod.rs` (add `pub mod orchestrate;`)
- Test: inline in `types.rs`

**Interfaces:**
```rust
pub const MAX_SUBTASKS: usize = 16;
pub const MAX_ORCHESTRATE_DEPTH: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrchestrateSubtask { pub prompt: String, #[serde(default)] pub label: Option<String> }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrateMode { Synthesize, Verify }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrchestrateInput {
    pub subtasks: Vec<OrchestrateSubtask>,
    #[serde(default)] pub synthesis_prompt: Option<String>,
    #[serde(default)] pub mode: Option<OrchestrateMode>,
    #[serde(default)] pub subagent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrateOutput {
    pub result: String, pub subtask_count: usize, pub succeeded: usize,
    pub skipped: usize, #[serde(skip_serializing_if = "Option::is_none")] pub verified: Option<usize>,
}
impl xai_tool_runtime::ToolOutput for OrchestrateOutput {}

/// Validate arg shape independent of any backend. Returns the sanitized subtasks.
pub fn validate(input: &OrchestrateInput) -> Result<(), String>;   // len 2..=MAX_SUBTASKS
```

- [ ] **Step 1: failing test** — `validate` rejects 1 and 17 sub-tasks, accepts 2 and 16; `label` defaults to None.
```rust
#[test]
fn validate_rejects_out_of_range_subtask_counts() {
    let mk = |n: usize| OrchestrateInput { subtasks: (0..n).map(|i| OrchestrateSubtask{prompt: format!("t{i}"), label: None}).collect(), synthesis_prompt: None, mode: None, subagent_type: None };
    assert!(validate(&mk(1)).is_err());
    assert!(validate(&mk(2)).is_ok());
    assert!(validate(&mk(16)).is_ok());
    assert!(validate(&mk(17)).is_err());
}
```
- [ ] **Step 2: run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-tools orchestrate::` — FAIL.
- [ ] **Step 3: implement** the types + `validate`.
- [ ] **Step 4: run** — PASS.
- [ ] **Step 5: commit** — `feat(orchestrate): input/output types and argument validation`.

---

### Task 2: stub-testable fan-out core (barrier + failure isolation)

**Files:**
- Create: `.../grok_build/orchestrate/fanout.rs`
- Test: inline (stub `SubagentBackend`)

**Interfaces:**
- Consumes: `SubagentBackend` (trait, `backend.rs`), `SubagentRequest`/`SubagentResult` (`task/types.rs`).
- Produces:
```rust
/// One resolved sub-task: the label and the sub-agent output, or None if it
/// failed/errored/cancelled (isolated, never fatal to the batch).
pub struct SubtaskOutcome { pub label: String, pub output: Option<String> }

/// Spawn every subtask concurrently on `backend`, barrier on all, isolate
/// failures. `base` carries the shared request fields (session id, prompt id,
/// subagent_type, cwd) mirrored from Task's request build.
pub async fn fan_out(
    backend: &dyn SubagentBackend,
    base: &RequestBase,
    subtasks: &[OrchestrateSubtask],
) -> Vec<SubtaskOutcome>;

pub struct RequestBase { pub subagent_type: String, pub parent_session_id: String, pub parent_prompt_id: Option<String>, pub cwd: Option<String> }
```
Implementation: `futures::future::join_all(subtasks.iter().enumerate().map(|(i, st)| spawn_one(backend, base, st, i)))`. `spawn_one` builds a `SubagentRequest` (mirror `task/mod.rs:294-321`: fresh `oneshot` for `result_tx`, `run_in_background:false`, `surface_completion:false`, `fork_context:false`, unique `id`), calls `backend.spawn(req).await`, maps `Ok(r) if r.success => Some(r.output.to_string())`, else `None`.
> **OPEN:** copy the exact `SubagentRequest { .. }` field list from `task/mod.rs:294-321`; the struct has ~12 fields — do not guess, mirror them (defaults for the ones `Task` sets from its input).

- [ ] **Step 1: failing test** — a stub backend returning success for all → N `SubtaskOutcome`s with `Some(output)`; a stub failing index 1 → that one `None`, others `Some`.
```rust
struct StubBackend { /* Vec<Result<SubagentResult, ()>> by call order, or by prompt */ spawns: std::sync::atomic::AtomicUsize, script: Vec<bool> }
#[async_trait] impl SubagentBackend for StubBackend { async fn spawn(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError> { let i = self.spawns.fetch_add(1, SeqCst); Ok(mk_result(self.script[i], &format!("out-{}", req.prompt))) } /* validate_type/other trait methods: minimal */ }
#[tokio::test]
async fn fan_out_isolates_failures_and_barriers_all() {
    let backend = StubBackend{ script: vec![true, false, true], .. };
    let subs = vec![sub("a"), sub("b"), sub("c")];
    let out = fan_out(&backend, &base(), &subs).await;
    assert_eq!(out.len(), 3);
    assert!(out[0].output.is_some() && out[2].output.is_some());
    assert!(out[1].output.is_none(), "failed subtask isolated");
    assert_eq!(backend.spawns.load(SeqCst), 3, "all spawned (barrier)");
}
```
> **OPEN:** the `SubagentBackend` trait has methods beyond `spawn` (`validate_type`, query/cancel per `backend.rs:32-64`). The stub must implement all of them — give trivial impls; confirm the exact method set from `backend.rs`.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: commit** — `feat(orchestrate): concurrent fan-out with per-subtask failure isolation`.

---

### Task 3: gather — synthesize + verify

**Files:**
- Create: `.../grok_build/orchestrate/gather.rs`
- Test: inline (stub backend)

**Interfaces:**
```rust
/// Verify mode: spawn one verifier per surviving output; keep CONFIRMED.
pub async fn verify_filter(backend: &dyn SubagentBackend, base: &RequestBase, outcomes: Vec<SubtaskOutcome>) -> (Vec<SubtaskOutcome>, usize /*verified*/);

/// Synthesize: spawn ONE synthesizer with the labeled surviving outputs injected;
/// return its output, or (fallback) the labeled raw outputs if it fails.
pub async fn synthesize(backend: &dyn SubagentBackend, base: &RequestBase, prompt: &str, outcomes: &[SubtaskOutcome]) -> String;

/// Format labeled outputs when there is no synthesis step.
pub fn labeled_concat(outcomes: &[SubtaskOutcome]) -> String;
```
Verifier prompt: `"Adversarially verify the following result. Reply with exactly CONFIRMED or REFUTED (default REFUTED if unsure).\n\n{output}"`; CONFIRMED iff the sub-agent output contains "CONFIRMED" (case-insensitive) and not "REFUTED".
Synthesizer prompt: `format!("{synthesis_prompt}\n\n--- sub-task results ---\n{labeled}")`.

- [ ] **Step 1: failing tests** — (a) `verify_filter` with a stub that returns "REFUTED" for label "b" drops it and returns `verified == survivors-1`; (b) `synthesize` returns the synthesizer's output; if the synthesizer spawn fails, returns `labeled_concat`; (c) `labeled_concat` prefixes each with its label.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: commit** — `feat(orchestrate): verify-filter and synthesize gather steps`.

---

### Task 4: the `orchestrate` tool (ToolMetadata + Tool)

**Files:**
- Create: `.../grok_build/orchestrate/tool.rs`
- Modify: `orchestrate/mod.rs` (`pub mod tool;` + re-export `OrchestrateTool`, `ORCHESTRATE_TOOL_NAME`)
- Test: inline (stub backend inserted into `Resources`, via `test_ctx`)

**Interfaces:** mirror `ci_guard/tool.rs`: `ToolMetadata` (kind Other, namespace GrokBuild, description_template, requires_expr True), `xai_tool_runtime::Tool` with `id = "orchestrate"`, `capabilities { is_read_only:false, tool_scope: Write }`, and `run`:
1. `shared_resources(&ctx)`; read `SubagentBackendResource` (missing → `custom("missing_resource", ...)`), `SessionIdResource`, `CurrentPromptIdResource`, `SubagentDepthCounter`.
2. `validate(&input)` → `invalid_arguments` on error; depth `>= MAX_ORCHESTRATE_DEPTH` → `invalid_arguments`.
3. Build `RequestBase` (subagent_type = `input.subagent_type` or the Task default; session/prompt id from resources; cwd via `resolve_cwd`).
4. `let outcomes = fan_out(backend, &base, &input.subtasks).await;`
5. If `mode == Verify`: `(outcomes, verified) = verify_filter(...)`.
6. `result = match &input.synthesis_prompt { Some(p) => synthesize(...).await, None => labeled_concat(&outcomes) }`.
7. Return `OrchestrateOutput { result, subtask_count, succeeded, skipped, verified }`.

- [ ] **Step 1: failing test** — insert a stub backend into `Resources` (via `test_ctx`), call `run` with 3 sub-tasks + a synthesis prompt → assert the output reports `subtask_count==3`, `succeeded` matches the stub, and `result` is the synthesizer's text. A depth-at-cap `Resources` → `run` errors without spawning.
> **OPEN:** confirm how to insert a resource + build a `ToolCallContext` in a unit test — use `crate::types::tool_metadata::test_ctx(resources)` (seen in tool_metadata.rs) + `resources.insert(SubagentBackendResource(Arc::new(stub)))`.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: commit** — `feat(orchestrate): the orchestrate tool wiring fan-out + gather`.

---

### Task 5: registry + ToolInput/ToolOutput wiring

**Files (all mirror the `ci_guard` merge):**
- Modify: `registry/types.rs` — `b.register::<grok_build::OrchestrateTool>();`
- Modify: `grok_build/mod.rs` — `pub use orchestrate::tool::{ORCHESTRATE_TOOL_NAME, OrchestrateTool};`
- Modify: `types/tool_io.rs` — `Orchestrate(...::OrchestrateInput)` variant.
- Modify: `types/output.rs` — `Orchestrate(...::OrchestrateOutput)` variant + `to_prompt_format` arm (`ToolOutput::Orchestrate(o) => o.result.clone()`).
- Modify: `normalization.rs` — add `| ToolInput::Orchestrate(_)` to the `=> return None` group.
- Modify: `reminders/task_completion.rs` — add `| ToolOutput::Orchestrate(_)` to the `=> {}` group.

- [ ] **Step 1: run the tools lib build** — `CARGO_INCREMENTAL=0 cargo build -p xai-grok-tools` — expect non-exhaustive-match errors, add exactly the arms the compiler names (mirror `ci_guard`).
- [ ] **Step 2: run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-tools orchestrate` — PASS.
- [ ] **Step 3: shell build check** — `CARGO_INCREMENTAL=0 cargo build -p xai-grok-shell` — clean (catch-all arms; no shell code change).
- [ ] **Step 4: commit** — `feat(orchestrate): register tool and wire ToolInput/ToolOutput`.

---

### Task 6: docs

**Files:**
- Modify: `crates/codegen/xai-grok-pager/docs/user-guide/15-agent-mode.md` — an "Orchestrate (deterministic fan-out)" section: what it does, `subtasks`/`synthesis_prompt`/`mode`, determinism + failure isolation + no-new-capability, and that it complements (not replaces) agent-driven `Task`.

- [ ] **Step 1:** write the section. **Step 2: commit** — `docs(orchestrate): document the orchestrate fan-out/verify tool`.

---

## Self-Review
**Spec coverage (§3 architecture, §4 interface, §5 determinism/safety, §6 verify, §7 errors, §8 tests):** types+validate → T1; fan-out barrier+isolation → T2; verify+synthesize+labeled → T3; tool run + resources + depth guard → T4; registration/wiring → T5; docs → T6. Every spec section maps to a task.

**Placeholder scan:** the three `OPEN:` notes (exact `SubagentRequest` fields, the full `SubagentBackend` trait method set for the stub, and the `test_ctx`/resource-insert helper) are "confirm against real code before writing", not invented values.

**Type consistency:** `OrchestrateSubtask`/`OrchestrateInput`/`OrchestrateOutput` (T1) used in T2–T5; `SubtaskOutcome`/`RequestBase` (T2) consumed by T3/T4; `fan_out`/`verify_filter`/`synthesize`/`labeled_concat` names identical across T2–T4; tool name `"orchestrate"` consistent in T4/T5.
