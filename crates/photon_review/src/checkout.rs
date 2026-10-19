use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gpui::{App, AppContext as _, Task};
use util::{
    ResultExt as _,
    command::{Stdio, new_command},
};
use workspace::{AppState, SerializedWorkspaceLocation, WorkspaceDb};

use crate::PrKey;

// The empty helper resets the configured list so that e.g. osxkeychain cannot store gh's token.
// Hooks are disabled because a PR's repository must never run code during checkout.
const SAFE_GIT_CONFIG: [&str; 6] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "credential.helper=",
    "-c",
    "credential.helper=!gh auth git-credential",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

pub fn parse_remote_url(url: &str) -> Option<RepoId> {
    let url = url.trim();
    let (authority, path) = match url.split_once("://") {
        Some(("https" | "http" | "ssh", rest)) => rest.split_once('/')?,
        Some(_) => return None,
        None => {
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') {
                return None;
            }
            (authority, path)
        }
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split_once(':').map_or(host, |(host, _port)| host);
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    let (owner, repo) = path.split_once('/')?;
    let valid = |part: &str| !part.is_empty() && !part.contains(char::is_whitespace);
    if !valid(host) || !valid(owner) || !valid(repo) || repo.contains('/') {
        return None;
    }
    Some(RepoId {
        host: host.to_lowercase(),
        owner: owner.to_lowercase(),
        repo: repo.to_lowercase(),
    })
}

pub fn fetch_url(repo: &RepoId) -> String {
    format!("https://{}/{}/{}.git", repo.host, repo.owner, repo.repo)
}

pub fn review_root() -> PathBuf {
    paths::data_dir().join("pr_review")
}

pub fn worktree_path(root: &Path, key: &PrKey) -> PathBuf {
    root.join("worktrees")
        .join(&key.host)
        .join(&key.owner)
        .join(&key.repo)
        .join(key.number.to_string())
}

pub fn clone_path(root: &Path, repo: &RepoId) -> PathBuf {
    root.join("clones")
        .join(&repo.host)
        .join(&repo.owner)
        .join(&repo.repo)
}

/// Repositories of open projects plus recent local workspaces, excluding Photon's own
/// clones and worktrees.
pub fn candidate_repos(cx: &App) -> Task<Vec<PathBuf>> {
    let app_state = AppState::try_global(cx);
    let mut candidates = Vec::new();
    if let Some(app_state) = &app_state {
        for workspace in app_state.workspace_store.read(cx).workspaces() {
            let Some(workspace) = workspace.upgrade() else {
                continue;
            };
            let project = workspace.read(cx).project().read(cx);
            for repository in project.repositories(cx).values() {
                candidates.push(repository.read(cx).work_directory_abs_path.to_path_buf());
            }
        }
    }
    let database = WorkspaceDb::global(cx);
    let fs = app_state.map(|app_state| app_state.fs.clone());
    cx.background_spawn(async move {
        if let Some(fs) = fs
            && let Some(recent) = database
                .recent_project_workspaces(fs.as_ref())
                .await
                .log_err()
        {
            for workspace in recent {
                if matches!(workspace.location, SerializedWorkspaceLocation::Local) {
                    candidates.extend(workspace.paths.paths().iter().cloned());
                }
            }
        }
        let root = review_root();
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|path| !path.starts_with(&root) && seen.insert(path.clone()));
        candidates
    })
}

/// Returns the common git dir (the bare repository for a linked worktree) of the first
/// candidate that has a remote pointing at `repo`.
pub async fn find_clone(candidates: Vec<PathBuf>, repo: &RepoId) -> Option<PathBuf> {
    for candidate in candidates {
        let Some(common_dir) = git(
            &candidate,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await
        .ok()
        .filter(|common_dir| !common_dir.is_empty()) else {
            continue;
        };
        let Ok(remotes) = git(&candidate, &["remote", "-v"]).await else {
            continue;
        };
        let matches = remotes.lines().any(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(parse_remote_url)
                .is_some_and(|remote| &remote == repo)
        });
        if matches {
            return Some(PathBuf::from(common_dir));
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    pub repo_dir: PathBuf,
    pub worktree: PathBuf,
    pub head_sha: String,
    pub merge_base: String,
    /// The worktree now holds code it didn't hold before: it was created, or moved to a new head.
    pub head_changed: bool,
}

pub(crate) fn ensure_safe_segment(label: &str, segment: &str) -> Result<()> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.contains(['/', '\\', '\0'])
    {
        bail!("unsafe {label} {segment:?}");
    }
    Ok(())
}

pub(crate) async fn safe_git(dir: &Path, args: &[&str]) -> Result<String> {
    let arguments: Vec<&str> = SAFE_GIT_CONFIG.iter().chain(args).copied().collect();
    git(dir, &arguments).await
}

async fn common_git_dir(dir: &Path) -> Result<PathBuf> {
    let common_dir = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    Ok(std::fs::canonicalize(common_dir)?)
}

pub async fn prepare_checkout(
    key: &PrKey,
    base_ref: &str,
    fetch_url: &str,
    clone: Option<PathBuf>,
    root: &Path,
) -> Result<Checkout> {
    finish_checkout(fetch_checkout(key, base_ref, fetch_url, clone, root).await?).await
}

/// The pull request's refs, fetched into `refs/photon/pr/<N>/*` without touching the worktree.
#[derive(Clone, Debug)]
pub struct FetchedCheckout {
    pub repo_dir: PathBuf,
    pub worktree: PathBuf,
    pub head_sha: String,
    pub merge_base: String,
    worktree_exists: bool,
}

pub async fn fetch_checkout(
    key: &PrKey,
    base_ref: &str,
    fetch_url: &str,
    clone: Option<PathBuf>,
    root: &Path,
) -> Result<FetchedCheckout> {
    ensure_safe_segment("host", &key.host)?;
    ensure_safe_segment("owner", &key.owner)?;
    ensure_safe_segment("repo", &key.repo)?;

    let worktree = worktree_path(root, key);
    let worktree_exists = worktree.exists();
    if worktree_exists && !worktree.join(".git").exists() {
        // Without its own `.git`, git would walk up and operate on an enclosing repository.
        bail!("{} exists but is not a git worktree", worktree.display());
    }

    let mut repo_dir = match clone {
        Some(clone) => clone,
        None => {
            let repo = RepoId {
                host: key.host.to_lowercase(),
                owner: key.owner.to_lowercase(),
                repo: key.repo.to_lowercase(),
            };
            let target = clone_path(root, &repo);
            if !target.exists() {
                let Some(parent) = target.parent() else {
                    bail!("invalid clone path {}", target.display());
                };
                std::fs::create_dir_all(parent)?;
                let partial = parent.join(format!(".{}.partial", key.repo.to_lowercase()));
                if partial.exists() {
                    std::fs::remove_dir_all(&partial)?;
                }
                let cloned = safe_git(
                    parent,
                    &[
                        "clone",
                        "--bare",
                        "--filter=blob:none",
                        "--no-tags",
                        fetch_url,
                        &partial.to_string_lossy(),
                    ],
                )
                .await;
                if let Err(error) = cloned {
                    std::fs::remove_dir_all(&partial).log_err();
                    return Err(error);
                }
                std::fs::rename(&partial, &target)?;
            }
            target
        }
    };

    // An existing worktree belongs to one repository; operating on another would strand it.
    if worktree_exists {
        let worktree_repo = common_git_dir(&worktree).await?;
        if common_git_dir(&repo_dir).await? != worktree_repo {
            repo_dir = worktree_repo;
        }
    }

    let number = key.number;
    let head_ref = format!("refs/photon/pr/{number}/head");
    let base_private_ref = format!("refs/photon/pr/{number}/base");
    let head_refspec = format!("+refs/pull/{number}/head:{head_ref}");
    let base_refspec = format!("+refs/heads/{base_ref}:{base_private_ref}");
    safe_git(
        &repo_dir,
        &[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            fetch_url,
            &head_refspec,
            &base_refspec,
        ],
    )
    .await?;

    let head_sha = safe_git(&repo_dir, &["rev-parse", &head_ref]).await?;
    let merge_base = safe_git(&repo_dir, &["merge-base", &head_ref, &base_private_ref]).await?;
    Ok(FetchedCheckout {
        repo_dir,
        worktree,
        head_sha,
        merge_base,
        worktree_exists,
    })
}

/// Moves Photon's worktree to the fetched head, creating it if needed.
pub async fn finish_checkout(fetched: FetchedCheckout) -> Result<Checkout> {
    let FetchedCheckout {
        repo_dir,
        worktree,
        head_sha,
        merge_base,
        worktree_exists,
    } = fetched;
    let previous_head = if worktree_exists {
        safe_git(&worktree, &["rev-parse", "HEAD"]).await.log_err()
    } else {
        None
    };
    if worktree_exists {
        safe_git(&worktree, &["checkout", "--detach", &head_sha]).await?;
    } else {
        if let Some(parent) = worktree.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // `--force` only overrides git's missing-but-registered check for this Photon path.
        safe_git(
            &repo_dir,
            &[
                "worktree",
                "add",
                "--force",
                "--detach",
                &worktree.to_string_lossy(),
                &head_sha,
            ],
        )
        .await?;
    }

    Ok(Checkout {
        repo_dir,
        worktree,
        head_changed: previous_head.as_ref() != Some(&head_sha),
        head_sha,
        merge_base,
    })
}

/// Fails, leaving the worktree and refs in place, when the worktree has local changes.
/// Closed-PR cleanup then force-removes Photon's own worktree, discarding local edits there.
pub async fn cleanup_checkout(checkout_repo: &Path, key: &PrKey, worktree: &Path) -> Result<()> {
    if worktree.exists() {
        safe_git(
            checkout_repo,
            &["worktree", "remove", &worktree.to_string_lossy()],
        )
        .await?;
    }
    for name in ["head", "base", "reviewed"] {
        let reference = format!("refs/photon/pr/{}/{name}", key.number);
        safe_git(checkout_repo, &["update-ref", "-d", &reference]).await?;
    }
    Ok(())
}

/// For a worktree `cleanup_checkout` could not remove, such as one with deleted contents.
pub(crate) async fn force_remove_worktree(checkout_repo: &Path, worktree: &Path) -> Result<()> {
    safe_git(
        checkout_repo,
        &["worktree", "remove", "--force", &worktree.to_string_lossy()],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) static COMMAND_LOG: parking_lot::Mutex<Vec<Vec<String>>> =
    parking_lot::Mutex::new(Vec::new());

pub(crate) async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(git_raw(dir, args).await?.trim().to_string())
}

// Patches and file contents must keep their trailing whitespace, which `git` trims.
pub(crate) async fn git_raw(dir: &Path, args: &[&str]) -> Result<String> {
    #[cfg(test)]
    COMMAND_LOG.lock().push(
        ["-C".to_string(), dir.to_string_lossy().into_owned()]
            .into_iter()
            .chain(args.iter().map(|arg| arg.to_string()))
            .collect(),
    );
    let output = new_command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "synchronous fixture setup")]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    pub(crate) fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub(crate) struct Fixture {
        _temp: tempfile::TempDir,
        pub(crate) upstream: PathBuf,
        pub(crate) seed: PathBuf,
        pub(crate) user: PathBuf,
        pub(crate) root: PathBuf,
        pub(crate) main_sha: String,
        pub(crate) pr_sha: String,
    }

    impl Fixture {
        pub(crate) fn new() -> Option<Self> {
            if Command::new("git").arg("--version").output().is_err() {
                return None;
            }
            let temp = tempfile::tempdir().expect("tempdir");
            let base = temp.path().canonicalize().expect("canonical");
            let upstream = base.join("upstream.git");
            let seed = base.join("seed");
            let user = base.join("user");
            let root = base.join("root");
            std::fs::create_dir_all(&seed).expect("seed dir");
            run(&base, &["init", "--bare", "-b", "main", "upstream.git"]);
            run(&upstream, &["config", "uploadpack.allowFilter", "true"]);
            run(&seed, &["init", "-b", "main"]);
            std::fs::write(seed.join("a.txt"), "one\n").expect("write");
            run(&seed, &["add", "."]);
            run(&seed, &["commit", "-m", "base"]);
            let main_sha = run(&seed, &["rev-parse", "HEAD"]);
            run(&seed, &["checkout", "-b", "pr"]);
            std::fs::write(seed.join("a.txt"), "one\ntwo\n").expect("write");
            run(&seed, &["commit", "-am", "pr change"]);
            let pr_sha = run(&seed, &["rev-parse", "HEAD"]);
            let upstream_str = upstream.to_str().expect("utf8");
            run(&seed, &["push", upstream_str, "main", "pr"]);
            run(&upstream, &["update-ref", "refs/pull/7/head", &pr_sha]);
            run(&upstream, &["update-ref", "-d", "refs/heads/pr"]);

            run(&base, &["clone", upstream_str, "user"]);
            run(&user, &["remote", "rename", "origin", "upstream"]);
            run(&user, &["checkout", "-b", "feature"]);
            std::fs::write(user.join("a.txt"), "stashed\n").expect("write");
            run(&user, &["stash", "push"]);
            std::fs::write(user.join("a.txt"), "dirty\n").expect("write");
            std::fs::write(user.join("untracked.txt"), "untracked\n").expect("write");
            Some(Self {
                _temp: temp,
                upstream,
                seed,
                user,
                root,
                main_sha,
                pr_sha,
            })
        }

        pub(crate) fn url(&self) -> String {
            format!("file://{}", self.upstream.display())
        }

        pub(crate) fn key(&self) -> PrKey {
            PrKey {
                host: "github.com".into(),
                owner: "acme".into(),
                repo: "app".into(),
                number: 7,
            }
        }

        pub(crate) fn user_state(&self) -> Vec<String> {
            let refs = run(&self.user, &["for-each-ref"])
                .lines()
                .filter(|line| !line.contains("refs/photon/"))
                .collect::<Vec<_>>()
                .join("\n");
            vec![
                run(&self.user, &["symbolic-ref", "HEAD"]),
                run(&self.user, &["status", "--porcelain=v1", "-uall"]),
                run(&self.user, &["stash", "list"]),
                refs,
                format!("{:?}", std::fs::read(self.user.join(".git/index")).ok()),
                format!("{:?}", std::fs::read(self.user.join("a.txt")).ok()),
                format!("{:?}", std::fs::read(self.user.join("untracked.txt")).ok()),
                format!("{}", self.user.join(".git/FETCH_HEAD").exists()),
            ]
        }
    }

    #[test]
    fn remote_url_forms_match() {
        let expected = Some(RepoId {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "app".into(),
        });
        for url in [
            "https://github.com/acme/app",
            "https://github.com/acme/app.git",
            "https://github.com/acme/app/",
            "https://github.com/acme/app.git/",
            "http://github.com/acme/app.git",
            "git@github.com:acme/app.git",
            "git@github.com:acme/app",
            "ssh://git@github.com/acme/app.git",
            "ssh://git@github.com:22/acme/app.git",
            "https://GitHub.com/Acme/App.git",
        ] {
            assert_eq!(parse_remote_url(url), expected, "{url}");
        }
        assert_eq!(
            parse_remote_url("https://ghe.corp.example/acme/app.git"),
            Some(RepoId {
                host: "ghe.corp.example".into(),
                owner: "acme".into(),
                repo: "app".into(),
            })
        );
        assert_eq!(parse_remote_url("not a url"), None);
        assert_eq!(parse_remote_url("https://github.com/acme"), None);
        assert_eq!(parse_remote_url("/local/path/app.git"), None);
    }

    #[test]
    fn paths_are_under_root() {
        let root = Path::new("/r");
        let key = PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "app".into(),
            number: 7,
        };
        assert_eq!(
            worktree_path(root, &key),
            Path::new("/r/worktrees/github.com/acme/app/7")
        );
        let repo = parse_remote_url("https://github.com/acme/app").expect("parses");
        assert_eq!(
            clone_path(root, &repo),
            Path::new("/r/clones/github.com/acme/app")
        );
        assert_eq!(fetch_url(&repo), "https://github.com/acme/app.git");
    }

    #[gpui::test]
    async fn find_clone_picks_matching_remote(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        run(
            &fixture.user,
            &["remote", "add", "github", "git@github.com:Acme/App.git"],
        );
        let other = fixture.seed.clone();
        let repo = parse_remote_url("https://github.com/acme/app").expect("parses");
        let found = find_clone(vec![other.clone(), fixture.user.clone()], &repo).await;
        assert_eq!(found, Some(fixture.user.join(".git")));

        let stranger = parse_remote_url("https://github.com/acme/other").expect("parses");
        assert_eq!(
            find_clone(vec![other, fixture.user.clone()], &stranger).await,
            None
        );
    }

    #[gpui::test]
    async fn checkout_fetches_private_refs_and_creates_worktree(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let checkout = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("checkout");

        assert_eq!(checkout.head_sha, fixture.pr_sha);
        assert_eq!(checkout.merge_base, fixture.main_sha);
        assert_eq!(
            checkout.worktree,
            worktree_path(&fixture.root, &fixture.key())
        );
        assert_eq!(
            run(&fixture.user, &["rev-parse", "refs/photon/pr/7/head"]),
            fixture.pr_sha
        );
        assert_eq!(
            run(&fixture.user, &["rev-parse", "refs/photon/pr/7/base"]),
            fixture.main_sha
        );
        assert_eq!(
            run(&checkout.worktree, &["rev-parse", "HEAD"]),
            fixture.pr_sha
        );
        assert_eq!(
            run(&checkout.worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD"
        );

        let logged = COMMAND_LOG.lock().clone();
        let fetch = logged
            .iter()
            .find(|argv| argv.iter().any(|arg| arg == "fetch"))
            .expect("fetch ran");
        assert!(
            fetch
                .windows(2)
                .any(|pair| pair == ["-c", "credential.helper=!gh auth git-credential"])
        );
        let position = |needle: &str| fetch.iter().position(|arg| arg == needle);
        let reset = position("credential.helper=").expect("helper reset");
        let gh = position("credential.helper=!gh auth git-credential").expect("gh helper");
        assert!(reset < gh);
        assert_eq!(fetch[reset - 1], "-c");
        assert!(fetch.iter().any(|arg| arg == "core.hooksPath=/dev/null"));
        for flag in [
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
        ] {
            assert!(fetch.iter().any(|arg| arg == flag), "{flag}");
        }
    }

    #[gpui::test]
    async fn checkout_never_touches_user_checkout(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let before = fixture.user_state();
        let checkout = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("checkout");
        assert_eq!(fixture.user_state(), before);
        assert_eq!(before[0], "refs/heads/feature");
        assert!(before[2].contains("stash@{0}"));

        cleanup_checkout(&checkout.repo_dir, &fixture.key(), &checkout.worktree)
            .await
            .expect("cleanup");
        assert_eq!(fixture.user_state(), before);
    }

    #[gpui::test]
    async fn checkout_reuses_worktree_and_moves_to_new_head(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let first = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("first");

        run(&fixture.seed, &["checkout", "pr"]);
        std::fs::write(fixture.seed.join("a.txt"), "one\ntwo\nthree\n").expect("write");
        run(&fixture.seed, &["commit", "-am", "more"]);
        let newer = run(&fixture.seed, &["rev-parse", "HEAD"]);
        run(
            &fixture.seed,
            &["push", fixture.upstream.to_str().expect("utf8"), "pr"],
        );
        run(
            &fixture.upstream,
            &["update-ref", "refs/pull/7/head", &newer],
        );

        let second = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("second");
        assert_eq!(second.worktree, first.worktree);
        assert_eq!(second.head_sha, newer);
        assert_eq!(run(&second.worktree, &["rev-parse", "HEAD"]), newer);
        assert_eq!(second.merge_base, fixture.main_sha);
        assert!(
            first.head_changed,
            "a new worktree holds code the user hasn't trusted"
        );
        assert!(second.head_changed);

        let third = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("third");
        assert!(!third.head_changed);
    }

    #[gpui::test]
    async fn fetch_step_leaves_the_worktree_head_alone(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let first = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("first");
        run(&fixture.seed, &["checkout", "pr"]);
        std::fs::write(fixture.seed.join("a.txt"), "one\ntwo\nthree\n").expect("write");
        run(&fixture.seed, &["commit", "-am", "more"]);
        let newer = run(&fixture.seed, &["rev-parse", "HEAD"]);
        run(
            &fixture.seed,
            &["push", fixture.upstream.to_str().expect("utf8"), "pr"],
        );
        run(
            &fixture.upstream,
            &["update-ref", "refs/pull/7/head", &newer],
        );

        let fetched = fetch_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("fetched");
        assert_eq!(fetched.head_sha, newer);
        assert_eq!(run(&first.worktree, &["rev-parse", "HEAD"]), first.head_sha);

        let checkout = finish_checkout(fetched).await.expect("checkout");
        assert!(checkout.head_changed);
        assert_eq!(run(&checkout.worktree, &["rev-parse", "HEAD"]), newer);
    }

    #[gpui::test]
    async fn fork_pr_fetches_pull_ref(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&fixture.upstream)
                .args(["rev-parse", "--verify", "refs/heads/pr"])
                .output()
                .expect("git runs")
                .status
                .code()
                != Some(0)
        );
        let checkout = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("checkout");
        assert_eq!(checkout.head_sha, fixture.pr_sha);
    }

    #[gpui::test]
    async fn no_local_clone_makes_blobless_bare_clone(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let checkout =
            prepare_checkout(&fixture.key(), "main", &fixture.url(), None, &fixture.root)
                .await
                .expect("checkout");
        let repo = RepoId {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "app".into(),
        };
        assert_eq!(checkout.repo_dir, clone_path(&fixture.root, &repo));
        assert!(checkout.repo_dir.starts_with(fixture.root.join("clones")));
        assert_eq!(
            run(&checkout.repo_dir, &["rev-parse", "--is-bare-repository"]),
            "true"
        );
        assert_eq!(
            run(
                &checkout.repo_dir,
                &["config", "remote.origin.partialclonefilter"]
            ),
            "blob:none"
        );
        assert_eq!(checkout.head_sha, fixture.pr_sha);
        assert_eq!(
            run(&checkout.worktree, &["rev-parse", "HEAD"]),
            fixture.pr_sha
        );
    }

    #[gpui::test]
    async fn cleanup_removes_worktree_and_refs(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let checkout = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("checkout");
        run(
            &fixture.user,
            &["update-ref", "refs/photon/pr/7/reviewed", &fixture.pr_sha],
        );
        cleanup_checkout(&checkout.repo_dir, &fixture.key(), &checkout.worktree)
            .await
            .expect("cleanup");
        assert!(!checkout.worktree.exists());
        assert_eq!(run(&fixture.user, &["for-each-ref", "refs/photon/"]), "");
        cleanup_checkout(&checkout.repo_dir, &fixture.key(), &checkout.worktree)
            .await
            .expect("cleanup is idempotent");
    }

    #[gpui::test]
    async fn cleanup_keeps_dirty_worktree(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let checkout = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("checkout");
        std::fs::write(checkout.worktree.join("a.txt"), "edited\n").expect("write");
        cleanup_checkout(&checkout.repo_dir, &fixture.key(), &checkout.worktree)
            .await
            .expect_err("dirty worktree is kept");
        assert!(checkout.worktree.join("a.txt").exists());
        assert!(
            run(&fixture.user, &["for-each-ref", "refs/photon/"]).contains("refs/photon/pr/7/head")
        );
    }

    #[gpui::test]
    async fn cleanup_after_directory_deleted_still_removes_refs(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let checkout = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("checkout");
        std::fs::remove_dir_all(&checkout.worktree).expect("delete worktree");
        cleanup_checkout(&checkout.repo_dir, &fixture.key(), &checkout.worktree)
            .await
            .expect("cleanup");
        assert_eq!(run(&fixture.user, &["for-each-ref", "refs/photon/"]), "");
    }

    #[gpui::test]
    async fn checkout_recovers_when_worktree_directory_was_deleted(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let first = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("first");
        std::fs::remove_dir_all(&first.worktree).expect("delete worktree");
        let second = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("second");
        assert_eq!(
            run(&second.worktree, &["rev-parse", "HEAD"]),
            fixture.pr_sha
        );
    }

    #[gpui::test]
    async fn existing_worktree_keeps_its_own_repository(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let first = prepare_checkout(&fixture.key(), "main", &fixture.url(), None, &fixture.root)
            .await
            .expect("first");
        let second = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("second");
        assert_eq!(
            std::fs::canonicalize(&second.repo_dir).expect("canonical"),
            std::fs::canonicalize(&first.repo_dir).expect("canonical")
        );
        cleanup_checkout(&second.repo_dir, &fixture.key(), &second.worktree)
            .await
            .expect("cleanup");
        assert!(!second.worktree.exists());
        assert_eq!(run(&fixture.user, &["for-each-ref", "refs/photon/"]), "");
    }

    #[gpui::test]
    async fn hooks_never_run(cx: &mut gpui::TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let marker = fixture.root.parent().expect("parent").join("hook-ran");
        for name in ["post-checkout", "reference-transaction"] {
            let hook = fixture.user.join(".git/hooks").join(name);
            std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("mkdir");
            std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))
                .expect("write hook");
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let checkout = prepare_checkout(
            &fixture.key(),
            "main",
            &fixture.url(),
            Some(fixture.user.clone()),
            &fixture.root,
        )
        .await
        .expect("checkout");
        cleanup_checkout(&checkout.repo_dir, &fixture.key(), &checkout.worktree)
            .await
            .expect("cleanup");
        assert!(!marker.exists());
    }

    #[gpui::test]
    async fn stale_partial_clone_is_discarded(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let partial = fixture.root.join("clones/github.com/acme/.app.partial");
        std::fs::create_dir_all(&partial).expect("mkdir");
        std::fs::write(partial.join("junk"), "x").expect("write");
        let checkout =
            prepare_checkout(&fixture.key(), "main", &fixture.url(), None, &fixture.root)
                .await
                .expect("checkout");
        assert!(!partial.exists());
        assert_eq!(checkout.head_sha, fixture.pr_sha);

        let failing = PrKey {
            repo: "missing".into(),
            ..fixture.key()
        };
        prepare_checkout(
            &failing,
            "main",
            "file:///nonexistent/repo",
            None,
            &fixture.root,
        )
        .await
        .expect_err("clone fails");
        let repo = RepoId {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "missing".into(),
        };
        assert!(!clone_path(&fixture.root, &repo).exists());
    }

    #[gpui::test]
    async fn unsafe_path_segments_are_rejected(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let Some(fixture) = Fixture::new() else {
            return;
        };
        for (host, owner, repo) in [
            ("github.com", "..", "app"),
            ("github.com", "acme", "a/b"),
            ("", "acme", "app"),
            ("github.com", "acme", "."),
            ("github.com", "a\\b", "app"),
        ] {
            let key = PrKey {
                host: host.into(),
                owner: owner.into(),
                repo: repo.into(),
                number: 7,
            };
            prepare_checkout(&key, "main", &fixture.url(), None, &fixture.root)
                .await
                .expect_err("unsafe segment");
        }
        assert!(!fixture.root.exists());
    }
}
