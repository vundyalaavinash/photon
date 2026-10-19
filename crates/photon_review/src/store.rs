use std::path::PathBuf;

use anyhow::Result;
use db::sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection};

use crate::PrKey;

pub struct ReviewDb(ThreadSafeConnection);

impl Domain for ReviewDb {
    const NAME: &str = "photon_review";

    const MIGRATIONS: &[&str] = &["CREATE TABLE IF NOT EXISTS reviews(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            node_id TEXT NOT NULL,
            repo_dir TEXT NOT NULL,
            worktree TEXT NOT NULL,
            last_review_commit TEXT,
            PRIMARY KEY(host, owner, repo, number)
        ) STRICT;"];
}

db::static_connection!(ReviewDb, []);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked {
    pub key: PrKey,
    pub node_id: String,
    pub repo_dir: PathBuf,
    pub worktree: PathBuf,
}

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
}
