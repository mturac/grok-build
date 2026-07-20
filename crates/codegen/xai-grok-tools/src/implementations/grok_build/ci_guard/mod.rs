//! CI Guardian: watch a GitHub PR's CI and, on a confident failure, prepare a
//! fix on an isolated local branch — never pushing — then notify the human.
//!
//! See `docs/superpowers/plans/2026-07-20-ci-guardian-loop.md` and the design
//! spec `docs/superpowers/specs/2026-07-19-ci-guardian-design.md`.

pub mod classify;
pub mod gh;
pub mod types;
