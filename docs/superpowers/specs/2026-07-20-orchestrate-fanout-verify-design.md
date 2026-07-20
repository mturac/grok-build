# `orchestrate` — Deterministic Fan-out / Verify Tool

- **Status:** Approved design (pre-implementation)
- **Date:** 2026-07-20
- **Author:** mturac
- **Sub-project:** #9 (deterministic multi-agent orchestration)

## 1. Background & Motivation

grok-build already spawns sub-agents, but only **agent-driven**: the model calls
the `Task` tool (and `WaitTasks`) whenever it decides to, and the sub-agent
coordinator (`SubagentBackend`, injected as `SubagentBackendResource`; the
`SubagentCoordinator` tracking pending/active/completed) manages them. There is
no **deterministic**, single-call primitive that fans a task out to N sub-agents,
waits for all of them (a barrier), and gathers the results in one structured step.

`orchestrate` adds exactly that: one tool call whose structure — the number of
sub-tasks, their prompts, and an optional gather step — is fixed at call time and
executed deterministically. The model can compose it, but cannot change the
fan-out mid-flight. It is a thin, deterministic layer over the **existing**
`SubagentBackend::spawn`, not a new agent runtime.

## 2. Goals / Non-Goals

### Goals
- A single `orchestrate` tool that spawns N sub-tasks concurrently, barriers on
  all, and returns either the labeled raw outputs or a synthesized/verified result.
- Reuse the existing `SubagentBackend` / coordinator — no new spawn machinery,
  no new concurrency system (the coordinator's caps already apply).
- Deterministic shape: exactly the given sub-tasks are spawned; the tool waits
  for all and gathers. A failed sub-task degrades to "skipped", never aborts the
  whole call.

### Non-Goals (deferred)
- A scripted/DSL workflow engine (pipelines, loops, conditionals) — that was the
  rejected larger option.
- Cross-machine / remote orchestration (the `RemoteBackend` is future work).
- New capabilities: sub-agents inherit the parent's capabilities; `orchestrate`
  grants nothing new (no push/exec/fs beyond what the parent already has).
- Streaming partial results to the client mid-run (results are gathered at the
  barrier).

## 3. Architecture

```
orchestrate(subtasks, synthesis_prompt?, mode?)
   │
   ├─ get SubagentBackendResource, SessionIdResource, CurrentPromptIdResource, SubagentDepthCounter
   ├─ depth guard: refuse if SubagentDepthCounter >= MAX_ORCHESTRATE_DEPTH
   │
   ├─ FAN-OUT (barrier): join_all over subtasks →
   │     backend.spawn(SubagentRequest{ prompt, run_in_background:false,
   │                                     surface_completion:false, ... })
   │     each → SubagentResult{ success, output, error, ... }
   │     failed/errored/cancelled → None (skipped, not fatal)
   │
   ├─ mode == "verify": for each successful output, spawn a verifier sub-agent
   │     ("Adversarially verify … reply CONFIRMED or REFUTED"); keep CONFIRMED
   │
   └─ GATHER:
         if synthesis_prompt: spawn ONE synthesizer sub-agent with the labeled
             (surviving) outputs injected + synthesis_prompt → return its output
         else: return the labeled outputs concatenated
```

All spawning goes through `backend().spawn(request).await`. Concurrency is bounded
by the coordinator that already backs `Task`; `orchestrate` simply submits all
sub-tasks and awaits them together — excess beyond the coordinator's cap queues
there, exactly as today.

## 4. Tool Interface

```rust
struct OrchestrateSubtask { prompt: String, label: Option<String> }

enum OrchestrateMode { Synthesize, Verify }   // default: Synthesize

struct OrchestrateInput {
    subtasks: Vec<OrchestrateSubtask>,          // 2..=MAX_SUBTASKS
    synthesis_prompt: Option<String>,           // gather step; none => raw labeled outputs
    mode: Option<OrchestrateMode>,              // verify => adversarial filter before gather
    subagent_type: Option<String>,              // default inherits the Task default type
}

struct OrchestrateOutput {
    result: String,                             // synthesized text OR concatenated labeled outputs
    subtask_count: usize,
    succeeded: usize,
    skipped: usize,                             // failed/errored/refused sub-tasks
    verified: Option<usize>,                    // Some(n) in verify mode
}
```

- `subtasks` must have **2..=MAX_SUBTASKS** entries (`MAX_SUBTASKS = 16`). Fewer
  than 2 is an `invalid_arguments` error (use `Task` for one).
- Each sub-task's `label` (or `subtask N`) prefixes its output in the gather step
  so the synthesizer can attribute.

## 5. Determinism & Safety

- **Deterministic shape:** the fan-out count and prompts are fixed by the input;
  the tool spawns exactly those and barriers. The model cannot add/remove
  sub-tasks after the call starts.
- **Failure isolation:** a sub-task whose `SubagentResult.success == false` (or
  errored/cancelled) becomes `None` and is skipped; the call still returns with
  the survivors. If *zero* survive, `result` explains that and `succeeded == 0`.
- **Recursion guard:** refuse when `SubagentDepthCounter >= MAX_ORCHESTRATE_DEPTH`
  (default 2) so orchestrate-within-orchestrate cannot fan out exponentially.
- **No new capability:** sub-agents run with `run_in_background:false`,
  `surface_completion:false` (intermediate sub-tasks don't spam the client), and
  the parent's capability set. `orchestrate` adds no push/exec/fs authority.
- **Cost visibility:** `OrchestrateOutput` reports `subtask_count/succeeded/
  skipped/verified` so the model and user can see the fan-out cost.

## 6. Verify Mode

When `mode == Verify`, each surviving sub-task output is handed to an independent
verifier sub-agent prompted to adversarially judge it and reply `CONFIRMED` or
`REFUTED` (default REFUTED when uncertain). Only CONFIRMED outputs reach the
gather step. This is the "N independent workers + adversarial check" pattern as a
one-call primitive. `verified` reports how many passed.

## 7. Error Handling

| Condition | Behavior |
|---|---|
| `subtasks.len() < 2` or `> MAX_SUBTASKS` | `invalid_arguments` error |
| `SubagentBackendResource` missing | `custom("missing_resource", ...)` (subagent support not initialized) — mirrors `Task` |
| Depth >= MAX_ORCHESTRATE_DEPTH | `invalid_arguments` "orchestrate nesting too deep" |
| A sub-task fails/errors/cancels | skipped (`None`); counted in `skipped` |
| All sub-tasks skipped | return with `succeeded == 0` and an explanatory `result` |
| Synthesizer sub-task fails | fall back to returning the labeled raw outputs |

## 8. Testing

Behind a stub `SubagentBackend` (the trait is already the seam), so the
orchestration logic is unit-tested with no real agents:

- **Fan-out barrier:** N sub-tasks → exactly N `spawn` calls; all awaited before
  gather; the synthesizer receives all N labeled outputs.
- **Failure isolation:** a stub returning `success:false` for one sub-task →
  that one is skipped, others gather, `skipped == 1`.
- **All-fail:** every sub-task fails → `succeeded == 0`, explanatory result, no
  synthesizer spawn (or a graceful empty synthesis).
- **Verify mode:** stub verifier REFUTES one → it's filtered; `verified` counts
  the CONFIRMED ones.
- **Depth guard:** depth at the cap → `invalid_arguments`, no spawns.
- **Arg validation:** 1 sub-task and 17 sub-tasks both rejected.
- **Synthesizer fallback:** synthesizer spawn fails → labeled raw outputs returned.

Registration + `ToolInput`/`ToolOutput` wiring mirrors `ci_guard` (derive_more
`From` variants + `normalization`/`to_prompt_format`/`task_completion` arms).

## 9. Open Questions
- Should `subagent_type` be per-subtask rather than one for the whole call?
  (Lean: one for MVP; per-subtask is a trivial later extension.)
- Default synthesizer prompt when `synthesis_prompt` is omitted but the model
  clearly wants a merge — MVP returns raw labeled outputs and lets the caller
  synthesize in its own turn. (Lean: keep MVP simple.)
