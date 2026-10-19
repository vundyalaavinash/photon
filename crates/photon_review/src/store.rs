use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use db::sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection};

use crate::{PrKey, Side};

pub struct ReviewDb(ThreadSafeConnection);

pub const SUMMARY_SLOT: &str = "summary";

pub fn reply_slot(thread_id: &str) -> String {
    format!("reply:{thread_id}")
}

impl Domain for ReviewDb {
    const NAME: &str = "photon_review";

    const MIGRATIONS: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS reviews(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            node_id TEXT NOT NULL,
            repo_dir TEXT NOT NULL,
            worktree TEXT NOT NULL,
            last_review_commit TEXT,
            PRIMARY KEY(host, owner, repo, number)
        ) STRICT;",
        "CREATE TABLE drafts(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            path TEXT NOT NULL,
            side TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            commit_sha TEXT NOT NULL,
            anchor_text TEXT NOT NULL,
            body TEXT NOT NULL,
            outdated INTEGER NOT NULL,
            general INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE viewed_steps(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            step_key TEXT NOT NULL,
            PRIMARY KEY(host, owner, repo, number, step_key)
        ) STRICT;",
        "ALTER TABLE drafts ADD COLUMN sending INTEGER NOT NULL DEFAULT 0;",
        "CREATE TABLE composer_texts(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            slot TEXT NOT NULL,
            body TEXT NOT NULL,
            PRIMARY KEY(host, owner, repo, number, slot)
        ) STRICT;",
    ];
}

db::static_connection!(ReviewDb, []);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked {
    pub key: PrKey,
    pub node_id: String,
    pub repo_dir: PathBuf,
    pub worktree: PathBuf,
}

/// A review comment the user is writing. Lines are 1-based; `Side::Left` lines are base lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub id: Option<i64>,
    pub key: PrKey,
    pub path: String,
    pub side: Side,
    pub start_line: u32,
    pub end_line: u32,
    pub commit: String,
    pub anchor_text: String,
    pub body: String,
    pub outdated: bool,
    pub general: bool,
    pub post_state: PostState,
}

/// Stored in the `sending` column as 0, 1 or 2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PostState {
    #[default]
    Unposted,
    /// Set while Submit posts the draft. Still set after a crash or a failure the review didn't
    /// see, so the draft may already be on GitHub.
    Sending,
    /// Edited while its review was posting: the earlier text is on GitHub, this one isn't.
    EarlierVersionPosted,
}

impl PostState {
    fn from_column(value: i64) -> Self {
        match value {
            1 => PostState::Sending,
            2 => PostState::EarlierVersionPosted,
            _ => PostState::Unposted,
        }
    }

    fn to_column(self) -> i64 {
        match self {
            PostState::Unposted => 0,
            PostState::Sending => 1,
            PostState::EarlierVersionPosted => 2,
        }
    }
}

/// Saves that fail before reaching the database, for testing the retry path.
#[cfg(test)]
pub(crate) static FAILING_SAVES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

type DraftRow = (
    (i64, String, String, i64, i64),
    (String, String, String, bool, bool, i64),
);

type TrackedRow = (String, String, String, i64, String, String, String);

impl ReviewDb {
    /// Keeps `last_review_commit` when the pull request is already tracked.
    pub async fn track(&self, tracked: Tracked) -> Result<()> {
        let number = i64::try_from(tracked.key.number)?;
        self.write(move |connection| {
            connection.exec_bound::<TrackedRow>(
                "INSERT INTO reviews(host, owner, repo, number, node_id, repo_dir, worktree)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(host, owner, repo, number) DO UPDATE SET
                    node_id = excluded.node_id,
                    repo_dir = excluded.repo_dir,
                    worktree = excluded.worktree",
            )?((
                tracked.key.host,
                tracked.key.owner,
                tracked.key.repo,
                number,
                tracked.node_id,
                tracked.repo_dir.to_string_lossy().into_owned(),
                tracked.worktree.to_string_lossy().into_owned(),
            ))
        })
        .await
    }

    pub fn tracked(&self) -> Result<Vec<Tracked>> {
        self.select::<TrackedRow>(
            "SELECT host, owner, repo, number, node_id, repo_dir, worktree FROM reviews
             ORDER BY host, owner, repo, number",
        )?()?
        .into_iter()
        .map(|(host, owner, repo, number, node_id, repo_dir, worktree)| {
            Ok(Tracked {
                key: PrKey {
                    host,
                    owner,
                    repo,
                    number: u64::try_from(number)?,
                },
                node_id,
                repo_dir: PathBuf::from(repo_dir),
                worktree: PathBuf::from(worktree),
            })
        })
        .collect()
    }

    pub async fn untrack(&self, key: PrKey) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| {
            connection.exec_bound::<(String, String, String, i64)>(
                "DELETE FROM reviews WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((key.host, key.owner, key.repo, number))
        })
        .await
    }

    pub fn last_review(&self, key: &PrKey) -> Result<Option<String>> {
        let number = i64::try_from(key.number)?;
        let rows = self.select_bound::<(String, String, String, i64), Option<String>>(
            "SELECT last_review_commit FROM reviews
             WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
        )?((
            key.host.clone(),
            key.owner.clone(),
            key.repo.clone(),
            number,
        ))?;
        Ok(rows.into_iter().next().flatten())
    }

    pub async fn set_last_review(&self, key: PrKey, commit: String) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| {
            connection.exec_bound::<(String, String, String, i64, String)>(
                "UPDATE reviews SET last_review_commit = ?5
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((key.host, key.owner, key.repo, number, commit))
        })
        .await
    }

    pub fn drafts(&self, key: &PrKey) -> Result<Vec<Draft>> {
        let number = i64::try_from(key.number)?;
        self.select_bound::<(String, String, String, i64), DraftRow>(
            "SELECT id, path, side, start_line, end_line,
                    commit_sha, anchor_text, body, outdated, general, sending
             FROM drafts WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4
             ORDER BY id",
        )?((
            key.host.clone(),
            key.owner.clone(),
            key.repo.clone(),
            number,
        ))?
        .into_iter()
        .map(
            |(
                (id, path, side, start_line, end_line),
                (commit, anchor_text, body, outdated, general, sending),
            )| {
                Ok(Draft {
                    id: Some(id),
                    key: key.clone(),
                    path,
                    side: match side.as_str() {
                        "LEFT" => Side::Left,
                        _ => Side::Right,
                    },
                    start_line: u32::try_from(start_line)?,
                    end_line: u32::try_from(end_line)?,
                    commit,
                    anchor_text,
                    body,
                    outdated,
                    general,
                    post_state: PostState::from_column(sending),
                })
            },
        )
        .collect()
    }

    /// Inserts a draft without an id, otherwise updates the row with its id. Returns the id.
    pub async fn save_draft(&self, draft: Draft) -> Result<i64> {
        #[cfg(test)]
        if FAILING_SAVES
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |count| count.checked_sub(1),
            )
            .is_ok()
        {
            anyhow::bail!("injected save failure");
        }
        let number = i64::try_from(draft.key.number)?;
        let side = match draft.side {
            Side::Left => "LEFT",
            Side::Right => "RIGHT",
        };
        let fields = (
            (
                draft.path,
                side.to_string(),
                i64::from(draft.start_line),
                i64::from(draft.end_line),
            ),
            (
                draft.commit,
                draft.anchor_text,
                draft.body,
                draft.outdated,
                draft.general,
                draft.post_state.to_column(),
            ),
        );
        let key = draft.key;
        self.write(move |connection| match draft.id {
            Some(id) => {
                connection.exec_bound::<(_, i64)>(
                    "UPDATE drafts SET path = ?1, side = ?2, start_line = ?3, end_line = ?4,
                        commit_sha = ?5, anchor_text = ?6, body = ?7, outdated = ?8, general = ?9,
                        sending = ?10
                     WHERE id = ?11",
                )?((fields, id))?;
                Ok(id)
            }
            None => connection.select_row_bound::<_, i64>(
                "INSERT INTO drafts(host, owner, repo, number, path, side, start_line,
                        end_line, commit_sha, anchor_text, body, outdated, general, sending)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                     RETURNING id",
            )?(((key.host, key.owner, key.repo, number), fields))?
            .context("insert returned no id"),
        })
        .await
    }

    pub async fn delete_draft(&self, id: i64) -> Result<()> {
        self.delete_drafts(vec![id]).await
    }

    pub async fn delete_drafts(&self, ids: Vec<i64>) -> Result<()> {
        self.write(move |connection| {
            let mut delete = connection.exec_bound::<i64>("DELETE FROM drafts WHERE id = ?1")?;
            for id in ids {
                delete(id)?;
            }
            Ok(())
        })
        .await
    }

    /// Unsent composer text by slot: `SUMMARY_SLOT`, or `reply_slot(thread_id)`.
    pub fn composer_texts(&self, key: &PrKey) -> Result<HashMap<String, String>> {
        let number = i64::try_from(key.number)?;
        Ok(
            self.select_bound::<(String, String, String, i64), (String, String)>(
                "SELECT slot, body FROM composer_texts
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((
                key.host.clone(),
                key.owner.clone(),
                key.repo.clone(),
                number,
            ))?
            .into_iter()
            .collect(),
        )
    }

    /// Blank text removes the slot's row.
    pub async fn save_composer_text(&self, key: PrKey, slot: String, body: String) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| {
            if body.trim().is_empty() {
                connection.exec_bound::<(String, String, String, i64, String)>(
                    "DELETE FROM composer_texts
                     WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4 AND slot = ?5",
                )?((key.host, key.owner, key.repo, number, slot))
            } else {
                connection.exec_bound::<(String, String, String, i64, String, String)>(
                    "INSERT INTO composer_texts(host, owner, repo, number, slot, body)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(host, owner, repo, number, slot) DO UPDATE SET body = excluded.body",
                )?((key.host, key.owner, key.repo, number, slot, body))
            }
        })
        .await
    }

    pub fn viewed_steps(&self, key: &PrKey) -> Result<HashSet<String>> {
        let number = i64::try_from(key.number)?;
        Ok(self.select_bound::<(String, String, String, i64), String>(
            "SELECT step_key FROM viewed_steps
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
        )?((
            key.host.clone(),
            key.owner.clone(),
            key.repo.clone(),
            number,
        ))?
        .into_iter()
        .collect())
    }

    pub async fn set_step_viewed(&self, key: PrKey, step_key: String, viewed: bool) -> Result<()> {
        let number = i64::try_from(key.number)?;
        let query = if viewed {
            "INSERT OR IGNORE INTO viewed_steps(host, owner, repo, number, step_key)
             VALUES (?1, ?2, ?3, ?4, ?5)"
        } else {
            "DELETE FROM viewed_steps
             WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4 AND step_key = ?5"
        };
        self.write(move |connection| {
            connection.exec_bound::<(String, String, String, i64, String)>(query)?((
                key.host, key.owner, key.repo, number, step_key,
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(number: u64) -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "app".into(),
            number,
        }
    }

    fn tracked(number: u64) -> Tracked {
        Tracked {
            key: key(number),
            node_id: format!("PR_{number}"),
            repo_dir: PathBuf::from("/repos/app"),
            worktree: PathBuf::from(format!("/wt/{number}")),
        }
    }

    #[gpui::test]
    async fn tracked_and_last_review_round_trip(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("tracked_round_trip"));
        assert_eq!(db.tracked().unwrap(), vec![]);
        assert_eq!(db.last_review(&key(7)).unwrap(), None);

        db.track(tracked(7)).await.unwrap();
        db.track(tracked(9)).await.unwrap();
        assert_eq!(db.tracked().unwrap(), vec![tracked(7), tracked(9)]);
        assert_eq!(db.last_review(&key(7)).unwrap(), None);

        db.set_last_review(key(7), "abc123".into()).await.unwrap();
        assert_eq!(db.last_review(&key(7)).unwrap().as_deref(), Some("abc123"));

        let mut moved = tracked(7);
        moved.worktree = PathBuf::from("/wt/moved");
        db.track(moved.clone()).await.unwrap();
        assert_eq!(db.tracked().unwrap()[0], moved);
        assert_eq!(db.last_review(&key(7)).unwrap().as_deref(), Some("abc123"));

        db.untrack(key(7)).await.unwrap();
        assert_eq!(db.tracked().unwrap(), vec![tracked(9)]);
        assert_eq!(db.last_review(&key(7)).unwrap(), None);
    }

    #[gpui::test]
    async fn composer_texts_round_trip(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("composer_texts_round_trip"));
        db.save_composer_text(key(7), "summary".into(), "Looks good".into())
            .await
            .unwrap();
        db.save_composer_text(key(7), "reply:PRRT_1".into(), "Agreed".into())
            .await
            .unwrap();
        db.save_composer_text(key(9), "summary".into(), "Other".into())
            .await
            .unwrap();
        db.save_composer_text(key(7), "summary".into(), "Looks great".into())
            .await
            .unwrap();
        let texts = db.composer_texts(&key(7)).unwrap();
        assert_eq!(
            texts.get("summary").map(String::as_str),
            Some("Looks great")
        );
        assert_eq!(
            texts.get("reply:PRRT_1").map(String::as_str),
            Some("Agreed")
        );
        assert_eq!(texts.len(), 2);

        db.save_composer_text(key(7), "summary".into(), "  ".into())
            .await
            .unwrap();
        let texts = db.composer_texts(&key(7)).unwrap();
        assert_eq!(texts.get("summary"), None);
        assert_eq!(texts.len(), 1);
    }

    fn draft(number: u64, path: &str, start_line: u32) -> Draft {
        Draft {
            id: None,
            key: key(number),
            path: path.into(),
            side: Side::Right,
            start_line,
            end_line: start_line + 1,
            commit: "abc".into(),
            anchor_text: "a\nb".into(),
            body: "Why?".into(),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
        }
    }

    #[gpui::test]
    async fn drafts_round_trip(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("drafts_round_trip"));
        assert_eq!(db.drafts(&key(7)).unwrap(), vec![]);

        let mut first = draft(7, "src/b.rs", 3);
        first.id = Some(db.save_draft(first.clone()).await.unwrap());
        let mut second = draft(7, "src/a.rs", 9);
        second.side = Side::Left;
        second.outdated = true;
        second.general = true;
        second.post_state = PostState::EarlierVersionPosted;
        second.id = Some(db.save_draft(second.clone()).await.unwrap());
        let mut other = draft(8, "src/a.rs", 1);
        other.id = Some(db.save_draft(other.clone()).await.unwrap());
        assert_ne!(first.id, second.id);
        assert_eq!(
            db.drafts(&key(7)).unwrap(),
            vec![first.clone(), second.clone()]
        );

        first.body = "Why not?".into();
        first.start_line = 4;
        first.commit = "def".into();
        assert_eq!(
            db.save_draft(first.clone()).await.unwrap(),
            first.id.unwrap()
        );
        assert_eq!(
            db.drafts(&key(7)).unwrap(),
            vec![first.clone(), second.clone()]
        );

        first.post_state = PostState::Sending;
        db.save_draft(first.clone()).await.unwrap();
        assert_eq!(
            db.drafts(&key(7)).unwrap()[0].post_state,
            PostState::Sending
        );
        first.post_state = PostState::Unposted;
        db.save_draft(first.clone()).await.unwrap();

        db.delete_draft(second.id.unwrap()).await.unwrap();
        assert_eq!(db.drafts(&key(7)).unwrap(), vec![first.clone()]);
        db.delete_drafts(vec![first.id.unwrap(), other.id.unwrap()])
            .await
            .unwrap();
        assert_eq!(db.drafts(&key(7)).unwrap(), vec![]);
        assert_eq!(db.drafts(&key(8)).unwrap(), vec![]);

        assert!(db.viewed_steps(&key(7)).unwrap().is_empty());
        db.set_step_viewed(key(7), "src/a.rs::f".into(), true)
            .await
            .unwrap();
        db.set_step_viewed(key(7), "src/a.rs::f".into(), true)
            .await
            .unwrap();
        db.set_step_viewed(key(7), "src/b.rs".into(), true)
            .await
            .unwrap();
        db.set_step_viewed(key(7), "src/b.rs".into(), false)
            .await
            .unwrap();
        assert_eq!(
            db.viewed_steps(&key(7)).unwrap(),
            HashSet::from_iter(["src/a.rs::f".to_string()])
        );
        assert!(db.viewed_steps(&key(8)).unwrap().is_empty());
    }
}
