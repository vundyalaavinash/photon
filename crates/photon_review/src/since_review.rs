use std::collections::BTreeMap;

use anyhow::{Result, bail};
use gpui::actions;

use crate::checkout::{git, safe_git};
use crate::{Checkout, FileDiff, PrKey, base_text, git_diff};

actions!(
    photon_review,
    [
        /// Shows only what changed since the commit you last reviewed.
        ToggleChangesSinceReview
    ]
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinceReviewPlan {
    Incremental {
        from: String,
    },
    Rebased {
        old_head: String,
        old_merge_base: String,
    },
    Unavailable {
        message: String,
    },
}

impl SinceReviewPlan {
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Unavailable { message } => Some(message),
            _ => None,
        }
    }
}

fn is_commit_id(reviewed: &str) -> bool {
    matches!(reviewed.len(), 40 | 64) && reviewed.chars().all(|c| c.is_ascii_hexdigit())
}

fn short(reviewed: &str) -> &str {
    reviewed.get(..7).unwrap_or(reviewed)
}

pub async fn plan_since_review(
    checkout: &Checkout,
    key: &PrKey,
    fetch_url: &str,
    reviewed: &str,
) -> SinceReviewPlan {
    // A value from the database must not reach git as an option or a revision expression.
    let result = if is_commit_id(reviewed) {
        plan(checkout, key, fetch_url, reviewed).await
    } else {
        Err(fetch_failed(reviewed))
    };
    result.unwrap_or_else(|message| SinceReviewPlan::Unavailable { message })
}

fn fetch_failed(reviewed: &str) -> String {
    format!(
        "Couldn't fetch the commit you last reviewed ({}). Showing the full diff.",
        short(reviewed)
    )
}

async fn plan(
    checkout: &Checkout,
    key: &PrKey,
    fetch_url: &str,
    reviewed: &str,
) -> Result<SinceReviewPlan, String> {
    let repo = &checkout.repo_dir;
    let number = key.number;
    let present = git(repo, &["cat-file", "-e", &format!("{reviewed}^{{commit}}")])
        .await
        .is_ok();
    if !present {
        // `+` because the ref may still hold an earlier reviewed commit.
        let refspec = format!("+{reviewed}:refs/photon/pr/{number}/reviewed");
        let fetched = safe_git(
            repo,
            &[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--no-auto-maintenance",
                fetch_url,
                &refspec,
            ],
        )
        .await;
        if let Err(error) = fetched {
            log::warn!("fetching the reviewed commit: {error:#}");
            return Err(fetch_failed(reviewed));
        }
    }

    let base_ref = format!("refs/photon/pr/{number}/base");
    let old_merge_base = git(repo, &["merge-base", reviewed, &base_ref])
        .await
        .map_err(|error| {
            log::warn!("comparing with the reviewed commit: {error:#}");
            format!(
                "Couldn't compare with the commit you last reviewed ({}). Showing the full diff.",
                short(reviewed)
            )
        })?;
    // Exit status 1 means "not an ancestor", which is the answer rather than a failure.
    let descends = git(
        repo,
        &["merge-base", "--is-ancestor", reviewed, &checkout.head_sha],
    )
    .await
    .is_ok();
    if old_merge_base == checkout.merge_base && descends {
        Ok(SinceReviewPlan::Incremental {
            from: reviewed.to_string(),
        })
    } else {
        Ok(SinceReviewPlan::Rebased {
            old_head: reviewed.to_string(),
            old_merge_base,
        })
    }
}

/// Paths whose hunk bodies differ between the versions, so a rebase that only shifts line numbers
/// is not the author's change.
pub fn author_changed_paths(old: &[FileDiff], new: &[FileDiff]) -> Vec<String> {
    let bodies = |files: &[FileDiff]| -> BTreeMap<String, Vec<String>> {
        files
            .iter()
            .map(|file| {
                let bodies = file.hunks.iter().map(|hunk| hunk.body.clone()).collect();
                (file.path.clone(), bodies)
            })
            .collect()
    };
    let (old, new) = (bodies(old), bodies(new));
    let mut paths: Vec<String> = old
        .iter()
        .filter(|(path, bodies)| new.get(*path) != Some(bodies))
        .map(|(path, _)| path.clone())
        .collect();
    paths.extend(new.keys().filter(|path| !old.contains_key(*path)).cloned());
    paths.sort();
    paths
}

/// Binary and submodule entries have no hunks to compare, so their blobs are compared instead.
async fn opaque_changed_paths(
    checkout: &Checkout,
    old_merge_base: &str,
    old_head: &str,
    old: &[FileDiff],
    new: &[FileDiff],
) -> Vec<String> {
    let blob = async |revision: &str, path: &str| {
        git(
            &checkout.worktree,
            &["rev-parse", &format!("{revision}:{path}")],
        )
        .await
        .ok()
    };
    let mut changed = Vec::new();
    for old_file in old {
        let Some(new_file) = new.iter().find(|file| file.path == old_file.path) else {
            continue;
        };
        let opaque = [old_file, new_file].iter().any(|file| file.is_opaque());
        if !opaque {
            continue;
        }
        let path = &old_file.path;
        let before = (blob(old_merge_base, path).await, blob(old_head, path).await);
        let after = (
            blob(&checkout.merge_base, path).await,
            blob(&checkout.head_sha, path).await,
        );
        if before != after {
            changed.push(path.clone());
        }
    }
    changed
}

// ponytail: within a listed file, base changes from the rebase still show. Upgrade to a per-file
// range-diff if that bites.
pub async fn since_review_files(
    checkout: &Checkout,
    plan: &SinceReviewPlan,
) -> Result<Vec<(FileDiff, Option<String>)>> {
    let worktree = &checkout.worktree;
    let (from, keep) = match plan {
        SinceReviewPlan::Incremental { from } => (from, None),
        SinceReviewPlan::Rebased {
            old_head,
            old_merge_base,
        } => {
            let old = git_diff(worktree, old_merge_base, old_head).await?;
            let new = git_diff(worktree, &checkout.merge_base, &checkout.head_sha).await?;
            let mut paths = author_changed_paths(&old, &new);
            paths
                .extend(opaque_changed_paths(checkout, old_merge_base, old_head, &old, &new).await);
            paths.sort();
            paths.dedup();
            (old_head, Some(paths))
        }
        SinceReviewPlan::Unavailable { message } => bail!("{message}"),
    };
    let mut files = Vec::new();
    for diff in git_diff(worktree, from, &checkout.head_sha).await? {
        if keep.as_ref().is_some_and(|keep| !keep.contains(&diff.path)) {
            continue;
        }
        let base = base_text(worktree, from, &diff).await?;
        files.push((diff, base));
    }
    Ok(files)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "synchronous fixture setup")]
mod tests {
    use super::*;
    use crate::{FileStatus, Hunk};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            // Separate repos share their "base" commit only if its timestamp matches too.
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Repo {
        _temp: tempfile::TempDir,
        dir: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let dir = temp.path().canonicalize().expect("canonical").join("repo");
            std::fs::create_dir_all(&dir).expect("dir");
            run(&dir, &["init", "-b", "main"]);
            for name in ["b", "c"] {
                std::fs::write(dir.join(format!("{name}.bin")), [0u8, 1, 2]).expect("write");
            }
            for name in ["x", "y", "z"] {
                let text: String = (1..=8).map(|n| format!("{name}{n}\n")).collect();
                std::fs::write(dir.join(format!("{name}.rs")), text).expect("write");
            }
            run(&dir, &["add", "."]);
            run(&dir, &["commit", "-m", "base"]);
            Self { _temp: temp, dir }
        }

        fn commit(&self, file: &str, line: &str, message: &str) -> String {
            let path = self.dir.join(file);
            let mut text = std::fs::read_to_string(&path).expect("read");
            text.push_str(line);
            text.push('\n');
            std::fs::write(&path, text).expect("write");
            run(&self.dir, &["commit", "-am", message]);
            self.sha("HEAD")
        }

        fn sha(&self, revision: &str) -> String {
            run(&self.dir, &["rev-parse", revision])
        }

        fn checkout(&self, head: &str, base: &str) -> Checkout {
            run(&self.dir, &["update-ref", "refs/photon/pr/7/base", base]);
            Checkout {
                repo_dir: self.dir.clone(),
                worktree: self.dir.clone(),
                head_sha: self.sha(head),
                merge_base: run(&self.dir, &["merge-base", head, base]),
                head_changed: false,
            }
        }

        fn url(&self) -> String {
            format!("file://{}", self.dir.display())
        }
    }

    fn key() -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "app".into(),
            number: 7,
        }
    }

    fn paths(files: &[(FileDiff, Option<String>)]) -> Vec<&str> {
        files.iter().map(|(diff, _)| diff.path.as_str()).collect()
    }

    #[gpui::test]
    async fn incremental_range_when_head_moved_forward(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let repo = Repo::new();
        let base = repo.sha("main");
        run(&repo.dir, &["checkout", "-b", "pr"]);
        let reviewed = repo.commit("y.rs", "y9", "first");
        repo.commit("z.rs", "z9", "second");
        let checkout = repo.checkout("pr", &base);

        let plan = plan_since_review(&checkout, &key(), &repo.url(), &reviewed).await;
        assert_eq!(
            plan,
            SinceReviewPlan::Incremental {
                from: reviewed.clone()
            }
        );
        let files = since_review_files(&checkout, &plan).await.expect("files");
        assert_eq!(paths(&files), vec!["z.rs"]);
        let z_at_reviewed = run(&repo.dir, &["show", &format!("{reviewed}:z.rs")]);
        assert_eq!(
            files[0].1.as_deref().map(str::trim),
            Some(z_at_reviewed.as_str())
        );
    }

    #[gpui::test]
    async fn rebase_diffs_each_version_against_its_merge_base(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let repo = Repo::new();
        let old_base = repo.sha("main");
        run(&repo.dir, &["checkout", "-b", "pr"]);
        let reviewed = repo.commit("y.rs", "y9", "y change");
        run(&repo.dir, &["checkout", "main"]);
        let new_base = repo.commit("x.rs", "x9", "unrelated base change");
        run(&repo.dir, &["checkout", "-b", "pr2"]);
        repo.commit("y.rs", "y9", "y change again");
        repo.commit("z.rs", "z9", "new change");
        let checkout = repo.checkout("pr2", "main");
        assert_ne!(old_base, new_base);

        let plan = plan_since_review(&checkout, &key(), &repo.url(), &reviewed).await;
        assert_eq!(
            plan,
            SinceReviewPlan::Rebased {
                old_head: reviewed,
                old_merge_base: old_base
            }
        );
        let files = since_review_files(&checkout, &plan).await.expect("files");
        assert_eq!(paths(&files), vec!["z.rs"]);
    }

    #[gpui::test]
    async fn merge_from_base_uses_rebased_plan(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let repo = Repo::new();
        run(&repo.dir, &["checkout", "-b", "pr"]);
        let reviewed = repo.commit("y.rs", "y9", "y change");
        run(&repo.dir, &["checkout", "main"]);
        repo.commit("x.rs", "x9", "base moves");
        run(&repo.dir, &["checkout", "pr"]);
        run(&repo.dir, &["merge", "--no-edit", "main"]);
        let checkout = repo.checkout("pr", "main");

        let plan = plan_since_review(&checkout, &key(), &repo.url(), &reviewed).await;
        assert!(matches!(plan, SinceReviewPlan::Rebased { .. }), "{plan:?}");
        let files = since_review_files(&checkout, &plan).await.expect("files");
        assert!(paths(&files).is_empty(), "{:?}", paths(&files));
    }

    #[gpui::test]
    async fn unfetchable_review_commit_falls_back_with_message(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let repo = Repo::new();
        run(&repo.dir, &["checkout", "-b", "pr"]);
        repo.commit("y.rs", "y9", "change");
        let checkout = repo.checkout("pr", "main");
        let before = run(&repo.dir, &["for-each-ref"]);
        let missing = format!("{}1", "0".repeat(39));

        let plan = plan_since_review(&checkout, &key(), &repo.url(), &missing).await;
        assert_eq!(
            plan,
            SinceReviewPlan::Unavailable {
                message:
                    "Couldn't fetch the commit you last reviewed (0000000). Showing the full diff."
                        .into()
            }
        );
        assert_eq!(run(&repo.dir, &["for-each-ref"]), before);
        assert!(since_review_files(&checkout, &plan).await.is_err());
    }

    #[gpui::test]
    async fn old_commit_is_fetched_into_the_private_ref(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let upstream = Repo::new();
        run(&upstream.dir, &["checkout", "-b", "old"]);
        let reviewed = upstream.commit("y.rs", "y9", "reviewed");
        let local = Repo::new();
        run(&local.dir, &["checkout", "-b", "pr"]);
        local.commit("z.rs", "z9", "new");
        let checkout = local.checkout("pr", "main");

        let plan = plan_since_review(&checkout, &key(), &upstream.url(), &reviewed).await;
        assert!(matches!(plan, SinceReviewPlan::Rebased { .. }), "{plan:?}");
        assert_eq!(local.sha("refs/photon/pr/7/reviewed"), reviewed);
        assert!(!local.dir.join(".git/FETCH_HEAD").exists());
    }

    #[gpui::test]
    async fn binary_changed_by_the_author_across_a_rebase_is_listed(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let repo = Repo::new();
        let write =
            |name: &str, bytes: &[u8]| std::fs::write(repo.dir.join(name), bytes).expect("write");
        run(&repo.dir, &["checkout", "-b", "pr"]);
        write("b.bin", &[0, 1, 9]);
        write("c.bin", &[0, 1, 8]);
        run(&repo.dir, &["commit", "-am", "binaries"]);
        let reviewed = repo.sha("HEAD");
        run(&repo.dir, &["checkout", "main"]);
        repo.commit("x.rs", "x9", "base moves");
        run(&repo.dir, &["checkout", "-b", "pr2"]);
        write("b.bin", &[0, 1, 7]);
        write("c.bin", &[0, 1, 8]);
        run(&repo.dir, &["commit", "-am", "binaries again"]);
        let checkout = repo.checkout("pr2", "main");

        let plan = plan_since_review(&checkout, &key(), &repo.url(), &reviewed).await;
        let files = since_review_files(&checkout, &plan).await.expect("files");
        assert_eq!(paths(&files), vec!["b.bin"]);
    }

    #[gpui::test]
    async fn refetching_a_different_commit_updates_the_private_ref(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let upstream = Repo::new();
        run(&upstream.dir, &["checkout", "-b", "one"]);
        let first = upstream.commit("y.rs", "y9", "first");
        run(&upstream.dir, &["checkout", "main"]);
        run(&upstream.dir, &["checkout", "-b", "two"]);
        let second = upstream.commit("z.rs", "z9", "second");
        let local = Repo::new();
        run(&local.dir, &["checkout", "-b", "pr"]);
        local.commit("x.rs", "x9", "new");
        let checkout = local.checkout("pr", "main");

        plan_since_review(&checkout, &key(), &upstream.url(), &first).await;
        assert_eq!(local.sha("refs/photon/pr/7/reviewed"), first);
        plan_since_review(&checkout, &key(), &upstream.url(), &second).await;
        assert_eq!(local.sha("refs/photon/pr/7/reviewed"), second);
    }

    fn file(path: &str, start: u32, body: &str) -> FileDiff {
        FileDiff {
            path: path.into(),
            status: FileStatus::Modified,
            binary: false,
            submodule: false,
            symlink: false,
            hunks: vec![Hunk {
                old_start: start,
                old_len: 1,
                new_start: start,
                new_len: 2,
                body: body.into(),
            }],
        }
    }

    #[test]
    fn author_changed_paths_compares_hunk_bodies() {
        let old = [
            file("shifted.rs", 3, "+a\n"),
            file("edited.rs", 1, "+a\n"),
            file("gone.rs", 1, "+a\n"),
        ];
        let new = [
            file("shifted.rs", 40, "+a\n"),
            file("edited.rs", 1, "+b\n"),
            file("added.rs", 1, "+a\n"),
        ];
        assert_eq!(
            author_changed_paths(&old, &new),
            vec!["added.rs", "edited.rs", "gone.rs"]
        );
    }
}
