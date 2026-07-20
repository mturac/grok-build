//! Prepare a CI fix on an isolated local branch — and NEVER push.
//!
//! This is the one place the CI Guardian writes to the repo, so its guards are
//! load-bearing:
//! - refuses a dirty worktree (never commits unrelated user changes),
//! - aborts if the PR head moved since diagnosis (the patch is against stale
//!   code),
//! - creates a dedicated `grok-ci-fix/<pr>-<sha>` branch off the diagnosed head,
//! - commits with the repo's own git identity and a neutral, caller-supplied
//!   message (never injects any authorship),
//! - has NO push path at all — the human pushes.
//!
//! The `git` CLI is used (house style in this crate; no `git2` dep here).

use std::path::{Component, Path, PathBuf};

/// A single file to write as part of the fix (create or overwrite).
#[derive(Debug, Clone)]
pub struct FileEdit {
    /// Path relative to the repo root.
    pub path: String,
    pub contents: String,
}

/// The prepared fix: a set of file edits to apply on the fix branch.
pub type Patch = Vec<FileEdit>;

/// Outcome of preparing a fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixOutcome {
    /// A fix branch + commit were prepared locally (never pushed).
    Prepared { branch: String, diff_summary: String },
    /// The worktree had uncommitted changes — refused to avoid mixing them in.
    RefusedDirtyWorktree,
    /// The PR head advanced since diagnosis — the patch is stale, aborted.
    StaleHeadAborted,
    /// The patch was empty — nothing to do.
    NoFix,
}

/// Resolve a patch's relative path to an absolute target INSIDE the repo,
/// rejecting anything that could escape it. Rejects absolute paths and any `..`
/// component up front (before touching the filesystem, so there is no TOCTOU
/// window). A hallucinated or hostile patch must never write outside the repo.
fn safe_target(repo_root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let rel_path = Path::new(rel);
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => anyhow::bail!("patch path escapes the repo: {rel:?}"),
            Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!("patch path must be relative, got absolute: {rel:?}")
            }
        }
    }
    Ok(repo_root.join(rel_path))
}

async fn run_git(repo: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?)
}

async fn git_ok(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = run_git(repo, args).await?;
    if !out.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Prepare a fix commit on an isolated branch. See the module docs for the
/// guarantees. `diagnosed_head` is the head SHA the patch was built against;
/// `commit_message` MUST be a neutral message from the caller (no attribution).
///
/// NOTE: the caller is responsible for spending the automation budget and
/// persisting `CiGuardState` BEFORE calling this, so a crash mid-commit cannot
/// silently repeat the attempt on restart.
pub async fn apply_fix(
    repo_path: &Path,
    pr: u64,
    diagnosed_head: &str,
    patch: &[FileEdit],
    commit_message: &str,
) -> anyhow::Result<FixOutcome> {
    if patch.is_empty() {
        return Ok(FixOutcome::NoFix);
    }

    // 1. Refuse a dirty worktree — never fold unrelated user changes into the fix.
    let status = git_ok(repo_path, &["status", "--porcelain"]).await?;
    if !status.trim().is_empty() {
        return Ok(FixOutcome::RefusedDirtyWorktree);
    }

    // 2. Abort if the PR head moved since diagnosis (stale patch).
    let current_head = git_ok(repo_path, &["rev-parse", "HEAD"]).await?;
    let current_head = current_head.trim();
    if current_head != diagnosed_head {
        return Ok(FixOutcome::StaleHeadAborted);
    }

    // 3. Validate ALL patch paths BEFORE any side effect (no branch, no writes)
    //    so an unsafe path can never leave a dangling branch or a partial write.
    let mut targets = Vec::with_capacity(patch.len());
    for edit in patch {
        targets.push((safe_target(repo_path, &edit.path)?, &edit.contents));
    }

    // 4. Create the isolated fix branch off the diagnosed head. Short SHA via
    //    `chars().take` (never a byte-boundary slice panic on odd input).
    let short: String = diagnosed_head.chars().take(8).collect();
    let branch = format!("grok-ci-fix/{pr}-{short}");
    git_ok(repo_path, &["checkout", "-b", &branch]).await?;

    // 5. Apply the pre-validated edits (create/overwrite files inside the repo).
    for (target, contents) in &targets {
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(target, contents).await?;
    }

    // 5. Stage and commit with the repo's own identity + the neutral message.
    //    NO authorship is injected here. There is deliberately NO push.
    git_ok(repo_path, &["add", "-A"]).await?;
    git_ok(repo_path, &["commit", "-m", commit_message]).await?;

    // A short diff summary for the notification.
    let diff_summary = git_ok(repo_path, &["show", "--stat", "--format=", "HEAD"])
        .await
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok(FixOutcome::Prepared {
        branch,
        diff_summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn init_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "Test User"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let out = run_git(p, &args).await.unwrap();
            assert!(out.status.success(), "git {args:?}");
        }
        tokio::fs::write(p.join("README.md"), "base\n").await.unwrap();
        git_ok(p, &["add", "-A"]).await.unwrap();
        git_ok(p, &["commit", "-m", "base"]).await.unwrap();
        let head = git_ok(p, &["rev-parse", "HEAD"]).await.unwrap();
        (dir, head.trim().to_string())
    }

    fn edit(path: &str, contents: &str) -> FileEdit {
        FileEdit {
            path: path.into(),
            contents: contents.into(),
        }
    }

    #[tokio::test]
    async fn empty_patch_is_nofix() {
        let (dir, head) = init_repo().await;
        let out = apply_fix(dir.path(), 7, &head, &[], "msg").await.unwrap();
        assert_eq!(out, FixOutcome::NoFix);
    }

    #[tokio::test]
    async fn rejects_path_traversal_without_side_effects() {
        let (dir, head) = init_repo().await;
        for bad in ["../escape.txt", "../../etc/pwned", "/abs/pwned", "a/../../b"] {
            let out = apply_fix(dir.path(), 7, &head, &[edit(bad, "x")], "msg").await;
            assert!(out.is_err(), "path {bad:?} must be rejected");
        }
        // No branch was created and we stayed on main — no side effects leaked.
        let branch = git_ok(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(branch.trim(), "main");
        let branches = git_ok(dir.path(), &["branch", "--list", "grok-ci-fix/*"])
            .await
            .unwrap();
        assert!(branches.trim().is_empty(), "no fix branch should exist");
    }

    #[tokio::test]
    async fn refuses_dirty_worktree() {
        let (dir, head) = init_repo().await;
        tokio::fs::write(dir.path().join("dirty.txt"), "x")
            .await
            .unwrap();
        let out = apply_fix(dir.path(), 7, &head, &vec![edit("f.txt", "new")], "msg")
            .await
            .unwrap();
        assert_eq!(out, FixOutcome::RefusedDirtyWorktree);
        // Still on the base branch — nothing was committed.
        let branch = git_ok(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(branch.trim(), "main");
    }

    #[tokio::test]
    async fn stale_head_aborts() {
        let (dir, _head) = init_repo().await;
        let out = apply_fix(
            dir.path(),
            7,
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            &vec![edit("f.txt", "new")],
            "msg",
        )
        .await
        .unwrap();
        assert_eq!(out, FixOutcome::StaleHeadAborted);
        let branch = git_ok(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(branch.trim(), "main", "must not have switched branches");
    }

    #[tokio::test]
    async fn prepares_fix_on_isolated_branch() {
        let (dir, head) = init_repo().await;
        let out = apply_fix(
            dir.path(),
            7,
            &head,
            &vec![edit("src/fix.txt", "content")],
            "ci: prepare fix for PR #7",
        )
        .await
        .unwrap();

        let branch = match out {
            FixOutcome::Prepared { branch, diff_summary } => {
                assert!(branch.starts_with("grok-ci-fix/7-"), "branch: {branch}");
                assert!(diff_summary.contains("fix.txt"), "summary: {diff_summary}");
                branch
            }
            other => panic!("expected Prepared, got {other:?}"),
        };

        // We are on the fix branch, the file is committed, and the worktree is clean.
        let cur = git_ok(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(cur.trim(), branch);
        assert!(dir.path().join("src/fix.txt").exists());
        let status = git_ok(dir.path(), &["status", "--porcelain"]).await.unwrap();
        assert!(status.trim().is_empty(), "worktree clean after commit");

        // Exactly one new commit on top of base, and the base branch is untouched.
        let count = git_ok(dir.path(), &["rev-list", "--count", "main..HEAD"])
            .await
            .unwrap();
        assert_eq!(count.trim(), "1");

        // No remote was ever configured — nothing could have been pushed.
        let remotes = git_ok(dir.path(), &["remote"]).await.unwrap();
        assert!(remotes.trim().is_empty());
    }
}
