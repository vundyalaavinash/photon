use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::RangeInclusive;

use anyhow::{Context as _, Result, anyhow};
use serde::Deserialize;

use crate::gh::{GhRequest, graphql_request, rest_request};
use crate::{Draft, Placement, place, quoted_body, review_body};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostAuth {
    pub host: String,
    pub login: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxGroup {
    ReviewRequested,
    Yours,
    Mentioned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinceReview {
    NotReviewed,
    UpToDate,
    NewCommits(usize),
    ForcePushed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrSummary {
    pub key: PrKey,
    pub node_id: String,
    pub title: String,
    pub author: String,
    pub url: String,
    pub head_sha: String,
    pub base_ref: String,
    pub checks: ChecksState,
    pub unresolved_threads: usize,
    pub last_review_commit: Option<String>,
    pub since_review: SinceReview,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inbox {
    pub sections: Vec<(InboxGroup, Vec<PrSummary>)>,
    pub closed_tracked: Vec<String>,
    /// Tracked pull requests GitHub no longer resolves (deleted, or access lost).
    pub gone_tracked: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadComment {
    pub database_id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub side: Side,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<ThreadComment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFile {
    pub path: String,
    pub viewed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrDetail {
    pub summary: PrSummary,
    pub state: PrState,
    pub additions: u64,
    pub deletions: u64,
    pub files: Vec<PrFile>,
    pub threads: Vec<ReviewThread>,
}

#[derive(Deserialize)]
struct RawAuthStatus {
    hosts: BTreeMap<String, Vec<RawAccount>>,
}

#[derive(Deserialize)]
struct RawAccount {
    state: String,
    #[serde(default)]
    active: bool,
    login: Option<String>,
}

pub fn auth_status_request() -> GhRequest {
    GhRequest {
        args: ["auth", "status", "--json", "hosts"]
            .map(String::from)
            .to_vec(),
        stdin: None,
    }
}

pub fn parse_auth_status(json: &str) -> Result<Vec<HostAuth>> {
    let status: RawAuthStatus = serde_json::from_str(json).context("parsing gh auth status")?;
    Ok(status
        .hosts
        .into_iter()
        .filter_map(|(host, accounts)| {
            let account = accounts
                .iter()
                .find(|account| account.active)
                .or(accounts.first())?;
            let login = if account.state == "success" {
                account.login.clone()
            } else {
                None
            };
            Some(HostAuth { host, login })
        })
        .collect())
}

#[derive(Deserialize)]
struct Response<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<RawError>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    message: String,
}

fn parse_response<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    let response: Response<T> = serde_json::from_str(json).context("parsing GitHub response")?;
    if let Some(error) = response.errors.first() {
        return Err(anyhow!("{}", error.message));
    }
    response.data.context("GitHub response had no data")
}

/// Like `parse_response`, but keeps whatever data came back alongside errors, as GitHub returns
/// for one unreadable node. Only for reads: a mutation with any error has failed.
fn parse_partial_response<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    let response: Response<serde_json::Value> =
        serde_json::from_str(json).context("parsing GitHub response")?;
    let first_error = || {
        response
            .errors
            .first()
            .map(|error| anyhow!("{}", error.message))
    };
    let Some(data) = response.data.clone().filter(|data| !data.is_null()) else {
        return Err(first_error().unwrap_or_else(|| anyhow!("GitHub response had no data")));
    };
    match serde_json::from_value(data) {
        Ok(parsed) => {
            for error in &response.errors {
                log::warn!("GitHub returned a partial result: {}", error.message);
            }
            Ok(parsed)
        }
        Err(error) => Err(first_error()
            .unwrap_or_else(|| anyhow::Error::new(error).context("parsing GitHub response"))),
    }
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<Option<T>>,
}

#[derive(Deserialize)]
struct RawLogin {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPullRequest {
    id: String,
    number: u64,
    title: String,
    url: String,
    author: Option<RawLogin>,
    head_ref_oid: String,
    base_ref_name: String,
    repository: RawRepository,
    commits: Nodes<RawCommitNode>,
    #[serde(rename = "lastCommit")]
    last_commit: Nodes<RawLastCommit>,
    review_threads: Nodes<RawThread>,
    reviews: Nodes<RawReview>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    files: Option<Nodes<RawFile>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLastCommit {
    commit: RawLastCommitRollup,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLastCommitRollup {
    status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
    state: String,
}

#[derive(Deserialize)]
struct RawReview {
    commit: Option<RawOid>,
}

#[derive(Deserialize)]
struct RawOid {
    oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    #[serde(default)]
    id: String,
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    #[serde(default)]
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    diff_side: Option<String>,
    comments: Option<Nodes<RawComment>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    database_id: Option<u64>,
    author: Option<RawLogin>,
    body: String,
    created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFile {
    path: String,
    viewer_viewed_state: String,
}

// GitHub reports deleted accounts as a null author.
const GHOST_LOGIN: &str = "ghost";

fn author_login(author: Option<&RawLogin>) -> String {
    author.map_or_else(|| GHOST_LOGIN.to_string(), |author| author.login.clone())
}

fn summarize(host: &str, key: Option<&PrKey>, raw: &RawPullRequest) -> Result<PrSummary> {
    let key = match key {
        Some(key) => key.clone(),
        None => {
            let (owner, repo) = raw
                .repository
                .name_with_owner
                .split_once('/')
                .with_context(|| format!("bad repository {}", raw.repository.name_with_owner))?;
            PrKey {
                host: host.to_string(),
                owner: owner.to_string(),
                repo: repo.to_string(),
                number: raw.number,
            }
        }
    };
    let commits: Vec<&RawCommit> = raw
        .commits
        .nodes
        .iter()
        .flatten()
        .map(|node| &node.commit)
        .collect();
    let checks = match raw
        .last_commit
        .nodes
        .iter()
        .flatten()
        .next_back()
        .and_then(|node| node.commit.status_check_rollup.as_ref())
        .map(|rollup| rollup.state.as_str())
    {
        Some("SUCCESS") => ChecksState::Passing,
        Some("FAILURE" | "ERROR") => ChecksState::Failing,
        Some("PENDING" | "EXPECTED") => ChecksState::Pending,
        _ => ChecksState::Unknown,
    };
    let last_review_commit = raw
        .reviews
        .nodes
        .iter()
        .flatten()
        .next_back()
        .and_then(|review| review.commit.as_ref())
        .map(|commit| commit.oid.clone());
    let since_review = match &last_review_commit {
        None => SinceReview::NotReviewed,
        Some(oid) if *oid == raw.head_ref_oid => SinceReview::UpToDate,
        Some(oid) => match commits.iter().position(|commit| commit.oid == *oid) {
            Some(index) => SinceReview::NewCommits(commits.len() - 1 - index),
            None => SinceReview::ForcePushed,
        },
    };
    Ok(PrSummary {
        key,
        node_id: raw.id.clone(),
        title: raw.title.clone(),
        author: author_login(raw.author.as_ref()),
        url: raw.url.clone(),
        head_sha: raw.head_ref_oid.clone(),
        base_ref: raw.base_ref_name.clone(),
        checks,
        unresolved_threads: raw
            .review_threads
            .nodes
            .iter()
            .flatten()
            .filter(|thread| !thread.is_resolved)
            .count(),
        last_review_commit,
        since_review,
    })
}

const PR_SUMMARY_FIELDS: &str = "id number title url author { login } headRefOid baseRefName \
    repository { nameWithOwner } \
    commits(last: 100) { nodes { commit { oid } } } \
    lastCommit: commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } \
    reviews(last: 1, author: $login, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) \
    { nodes { commit { oid } } }";

pub fn inbox_request(host: &str, login: &str, tracked_node_ids: &[String]) -> GhRequest {
    let search = |alias: &str, qualifier: &str| {
        format!(
            "{alias}: search(query: \"is:pr is:open archived:false {qualifier}\", type: ISSUE, first: 50) \
             {{ nodes {{ ... on PullRequest {{ {PR_SUMMARY_FIELDS} \
             reviewThreads(first: 100) {{ nodes {{ isResolved }} }} }} }} }}"
        )
    };
    let query = format!(
        "query($login: String!, $tracked: [ID!]!) {{ {} {} {} \
         nodes(ids: $tracked) {{ ... on PullRequest {{ id state }} }} }}",
        search("reviewRequested", "review-requested:@me"),
        search("yours", "author:@me"),
        search("mentioned", "mentions:@me"),
    );
    graphql_request(
        host,
        &query,
        serde_json::json!({ "login": login, "tracked": tracked_node_ids }),
    )
}

#[derive(Deserialize)]
struct RawInbox {
    #[serde(rename = "reviewRequested")]
    review_requested: Option<Nodes<RawPullRequest>>,
    yours: Option<Nodes<RawPullRequest>>,
    mentioned: Option<Nodes<RawPullRequest>>,
    #[serde(default)]
    nodes: Vec<Option<RawTracked>>,
}

#[derive(Deserialize)]
struct RawTracked {
    id: Option<String>,
    state: Option<String>,
}

/// `tracked_node_ids` are the ids `inbox_request` asked about, in order.
pub fn parse_inbox(host: &str, json: &str, tracked_node_ids: &[String]) -> Result<Inbox> {
    let raw: RawInbox = parse_partial_response(json)?;
    let mut seen = HashSet::new();
    let mut sections = Vec::new();
    for (group, nodes) in [
        (InboxGroup::ReviewRequested, raw.review_requested),
        (InboxGroup::Yours, raw.yours),
        (InboxGroup::Mentioned, raw.mentioned),
    ] {
        let mut rows = Vec::new();
        for pull_request in nodes.iter().flat_map(|nodes| nodes.nodes.iter().flatten()) {
            if seen.insert(pull_request.id.clone()) {
                rows.push(summarize(host, None, pull_request)?);
            }
        }
        sections.push((group, rows));
    }
    // `nodes(ids:)` answers in request order, with null for an id it can't resolve.
    let gone_tracked = if raw.nodes.len() == tracked_node_ids.len() {
        raw.nodes
            .iter()
            .zip(tracked_node_ids)
            .filter(|(node, _)| node.is_none())
            .map(|(_, id)| id.clone())
            .collect()
    } else {
        Vec::new()
    };
    let closed_tracked = raw
        .nodes
        .into_iter()
        .flatten()
        .filter(|node| matches!(node.state.as_deref(), Some("MERGED" | "CLOSED")))
        .filter_map(|node| node.id)
        .collect();
    Ok(Inbox {
        sections,
        closed_tracked,
        gone_tracked,
    })
}

pub fn detail_request(key: &PrKey, login: &str) -> GhRequest {
    let query = format!(
        "query($owner: String!, $name: String!, $number: Int!, $login: String!) {{ \
         repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ \
         {PR_SUMMARY_FIELDS} state additions deletions \
         files(first: 100) {{ nodes {{ path viewerViewedState }} }} \
         reviewThreads(first: 100) {{ nodes {{ id isResolved isOutdated path line startLine diffSide \
         comments(first: 100) {{ nodes {{ databaseId author {{ login }} body createdAt }} }} }} }} \
         }} }} }}"
    );
    graphql_request(
        &key.host,
        &query,
        serde_json::json!({
            "owner": key.owner,
            "name": key.repo,
            "number": key.number,
            "login": login,
        }),
    )
}

#[derive(Deserialize)]
struct RawDetail {
    repository: Option<RawDetailRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetailRepository {
    pull_request: Option<RawPullRequest>,
}

pub fn parse_detail(key: &PrKey, json: &str) -> Result<PrDetail> {
    let raw: RawDetail = parse_response(json)?;
    let pull_request = raw
        .repository
        .and_then(|repository| repository.pull_request)
        .context("pull request not found")?;
    let summary = summarize(&key.host, Some(key), &pull_request)?;
    let state = match pull_request.state.as_deref() {
        Some("MERGED") => PrState::Merged,
        Some("CLOSED") => PrState::Closed,
        _ => PrState::Open,
    };
    let files = pull_request
        .files
        .map(|files| {
            files
                .nodes
                .into_iter()
                .flatten()
                .map(|file| PrFile {
                    path: file.path,
                    viewed: file.viewer_viewed_state == "VIEWED",
                })
                .collect()
        })
        .unwrap_or_default();
    let threads = pull_request
        .review_threads
        .nodes
        .into_iter()
        .flatten()
        .map(|thread| ReviewThread {
            id: thread.id,
            path: thread.path,
            line: thread.line,
            start_line: thread.start_line,
            side: if thread.diff_side.as_deref() == Some("LEFT") {
                Side::Left
            } else {
                Side::Right
            },
            is_resolved: thread.is_resolved,
            is_outdated: thread.is_outdated,
            comments: thread
                .comments
                .map(|comments| {
                    comments
                        .nodes
                        .into_iter()
                        .flatten()
                        .filter_map(|comment| {
                            Some(ThreadComment {
                                database_id: comment.database_id?,
                                author: author_login(comment.author.as_ref()),
                                body: comment.body,
                                created_at: comment.created_at,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    Ok(PrDetail {
        summary,
        state,
        additions: pull_request.additions,
        deletions: pull_request.deletions,
        files,
        threads,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "LEFT",
        Side::Right => "RIGHT",
    }
}

/// Whether Submit sends this draft. Outdated drafts stay local until the user re-places them or
/// posts them as general comments; a draft not yet re-anchored to this head would land on the
/// wrong lines, so it waits too. A draft that may already be on GitHub waits for the user to
/// include it again.
pub fn is_sendable(draft: &Draft, head_sha: &str) -> bool {
    !draft.body.trim().is_empty()
        && draft.post_state == crate::PostState::Unposted
        && (draft.general || (!draft.outdated && draft.commit == head_sha))
}

pub fn review_request(
    key: &PrKey,
    head_sha: &str,
    event: ReviewEvent,
    summary: &str,
    drafts: &[Draft],
    commentable: &HashMap<String, Vec<RangeInclusive<u32>>>,
    language_for_path: &dyn Fn(&str) -> Option<String>,
) -> GhRequest {
    let mut comments = Vec::new();
    let mut body_drafts = Vec::new();
    for draft in drafts.iter().filter(|draft| is_sendable(draft, head_sha)) {
        // The quote links to the commit the lines were read at, which is the head unless the
        // draft is an outdated one posted as a general comment.
        let quoted = || {
            quoted_body(
                draft,
                key,
                &draft.commit,
                language_for_path(&draft.path).as_deref(),
            )
        };
        match place(draft, commentable) {
            Placement::Line {
                path,
                side,
                start_line,
                line,
            } => {
                let mut comment = serde_json::json!({
                    "path": path,
                    "body": draft.body,
                    "line": line,
                    "side": side_name(side),
                });
                if let Some(start_line) = start_line {
                    comment["start_line"] = start_line.into();
                    comment["start_side"] = side_name(side).into();
                }
                comments.push(comment);
            }
            Placement::NearestLine { path, line } => comments.push(serde_json::json!({
                "path": path,
                "line": line,
                "side": "RIGHT",
                "body": quoted(),
            })),
            Placement::ReviewBody { .. } => body_drafts.push((draft, quoted())),
        }
    }
    rest_request(
        &key.host,
        "POST",
        &format!(
            "repos/{}/{}/pulls/{}/reviews",
            key.owner, key.repo, key.number
        ),
        Some(serde_json::json!({
            "commit_id": head_sha,
            "event": event.as_str(),
            "body": review_body(summary, &body_drafts),
            "comments": comments,
        })),
    )
}

pub fn reply_request(key: &PrKey, comment_database_id: u64, body: &str) -> GhRequest {
    rest_request(
        &key.host,
        "POST",
        &format!(
            "repos/{}/{}/pulls/{}/comments/{comment_database_id}/replies",
            key.owner, key.repo, key.number
        ),
        Some(serde_json::json!({ "body": body })),
    )
}

pub fn resolve_request(host: &str, thread_id: &str, resolved: bool) -> GhRequest {
    let mutation = if resolved {
        "resolveReviewThread"
    } else {
        "unresolveReviewThread"
    };
    graphql_request(
        host,
        &format!(
            "mutation($threadId: ID!) {{ {mutation}(input: {{threadId: $threadId}}) \
             {{ thread {{ isResolved }} }} }}"
        ),
        serde_json::json!({ "threadId": thread_id }),
    )
}

pub fn viewed_request(host: &str, pr_node_id: &str, path: &str, viewed: bool) -> GhRequest {
    let mutation = if viewed {
        "markFileAsViewed"
    } else {
        "unmarkFileAsViewed"
    };
    graphql_request(
        host,
        &format!(
            "mutation($pullRequestId: ID!, $path: String!) {{ \
             {mutation}(input: {{pullRequestId: $pullRequestId, path: $path}}) \
             {{ clientMutationId }} }}"
        ),
        serde_json::json!({ "pullRequestId": pr_node_id, "path": path }),
    )
}

pub fn parse_mutation(json: &str) -> Result<()> {
    parse_response::<serde_json::Value>(json).map(|_| ())
}

#[derive(Deserialize)]
struct RawRestComment {
    id: u64,
    user: Option<RawLogin>,
    body: String,
    created_at: String,
}

pub fn parse_reply(json: &str) -> Result<ThreadComment> {
    let comment: RawRestComment = serde_json::from_str(json).context("parsing GitHub reply")?;
    Ok(ThreadComment {
        database_id: comment.id,
        author: author_login(comment.user.as_ref()),
        body: comment.body,
        created_at: comment.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INBOX: &str = include_str!("../test_fixtures/inbox.json");
    const DETAIL: &str = include_str!("../test_fixtures/detail.json");

    fn key() -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 482,
        }
    }

    fn find(inbox: &Inbox, number: u64) -> &PrSummary {
        inbox
            .sections
            .iter()
            .flat_map(|(_, rows)| rows)
            .find(|row| row.key.number == number)
            .expect("row")
    }

    #[test]
    fn auth_status_lists_hosts() {
        let mut hosts =
            parse_auth_status(include_str!("../test_fixtures/auth_status.json")).expect("parse");
        hosts.sort_by(|a, b| a.host.cmp(&b.host));
        assert_eq!(
            hosts,
            [
                HostAuth {
                    host: "ghe.corp".into(),
                    login: None
                },
                HostAuth {
                    host: "github.com".into(),
                    login: Some("alice".into())
                },
            ]
        );
        assert_eq!(
            auth_status_request().args,
            ["auth", "status", "--json", "hosts"]
        );
    }

    #[test]
    fn auth_status_logged_out_is_empty() {
        let hosts = parse_auth_status(include_str!("../test_fixtures/auth_status_logged_out.json"))
            .expect("parse");
        assert!(hosts.is_empty());
    }

    #[test]
    fn inbox_groups_rows_and_counts() {
        let inbox = parse_inbox("github.com", INBOX, &[]).expect("parse");
        let groups: Vec<_> = inbox.sections.iter().map(|(group, _)| *group).collect();
        assert_eq!(
            groups,
            [
                InboxGroup::ReviewRequested,
                InboxGroup::Yours,
                InboxGroup::Mentioned
            ]
        );
        let row = find(&inbox, 482);
        assert_eq!(row.title, "Add retry to uploader");
        assert_eq!(row.author, "alice");
        assert_eq!(row.key, key());
        assert_eq!(row.checks, ChecksState::Passing);
        assert_eq!(row.unresolved_threads, 2);
        assert_eq!(row.head_sha, "a1");
        assert_eq!(row.base_ref, "main");
        assert_eq!(find(&inbox, 483).checks, ChecksState::Pending);
        assert_eq!(find(&inbox, 484).checks, ChecksState::Failing);
        assert_eq!(find(&inbox, 485).checks, ChecksState::Unknown);
        assert_eq!(find(&inbox, 485).key.repo, "api");
    }

    #[test]
    fn inbox_dedupes_across_groups() {
        let inbox = parse_inbox("github.com", INBOX, &[]).expect("parse");
        let numbers = |index: usize| -> Vec<u64> {
            inbox.sections[index]
                .1
                .iter()
                .map(|row| row.key.number)
                .collect()
        };
        assert_eq!(numbers(0), [482, 483, 484, 485]);
        assert_eq!(numbers(1), [490]);
        assert_eq!(numbers(2), [500]);
    }

    #[test]
    fn since_review_counts() {
        let inbox = parse_inbox("github.com", INBOX, &[]).expect("parse");
        assert_eq!(find(&inbox, 482).since_review, SinceReview::NotReviewed);
        assert_eq!(find(&inbox, 483).since_review, SinceReview::UpToDate);
        assert_eq!(find(&inbox, 484).since_review, SinceReview::NewCommits(3));
        assert_eq!(find(&inbox, 484).last_review_commit.as_deref(), Some("c2"));
        assert_eq!(find(&inbox, 485).since_review, SinceReview::ForcePushed);
    }

    #[test]
    fn inbox_reports_closed_tracked() {
        let inbox = parse_inbox("github.com", INBOX, &[]).expect("parse");
        assert_eq!(inbox.closed_tracked, ["PR_x"]);
    }

    #[test]
    fn detail_threads_files_and_viewed() {
        let detail = parse_detail(&key(), DETAIL).expect("parse");
        assert_eq!(detail.state, PrState::Open);
        assert_eq!((detail.additions, detail.deletions), (120, 14));
        assert_eq!(detail.summary.since_review, SinceReview::NewCommits(1));
        assert_eq!(detail.summary.unresolved_threads, 1);
        assert_eq!(
            detail.files,
            [
                PrFile {
                    path: "src/upload.rs".into(),
                    viewed: true
                },
                PrFile {
                    path: "src/retry.rs".into(),
                    viewed: false
                },
            ]
        );
        let thread = &detail.threads[0];
        assert_eq!(thread.id, "PRRT_1");
        assert_eq!((thread.line, thread.start_line), (Some(42), Some(40)));
        assert_eq!(thread.side, Side::Right);
        assert!(!thread.is_resolved && !thread.is_outdated);
        // The fixture's third comment has a null databaseId and is skipped.
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[0].database_id, 1001);
        assert_eq!(thread.comments[0].author, "bob");
        assert_eq!(thread.comments[1].author, "ghost");
        let outdated = &detail.threads[1];
        assert_eq!(outdated.line, None);
        assert_eq!(outdated.side, Side::Left);
        assert!(outdated.is_resolved && outdated.is_outdated);
    }

    #[test]
    fn inbox_partial_errors_keep_the_data() {
        let mut partial: serde_json::Value = serde_json::from_str(INBOX).expect("fixture");
        partial["data"]["mentioned"] = serde_json::Value::Null;
        partial["errors"] = serde_json::json!([{ "message": "Could not resolve PR_gone" }]);
        let tracked = ["PR_x".to_string(), "PR_y".into(), "PR_gone".into()];
        let inbox = parse_inbox("github.com", &partial.to_string(), &tracked).expect("partial");
        assert!(!inbox.sections[0].1.is_empty());
        assert!(inbox.sections[2].1.is_empty());
        assert_eq!(inbox.closed_tracked, ["PR_x"]);
        assert_eq!(inbox.gone_tracked, ["PR_gone"]);
        let failed_mutation =
            r#"{"data":{"addPullRequestReview":null},"errors":[{"message":"no"}]}"#;
        assert!(parse_mutation(failed_mutation).is_err());
    }

    #[test]
    fn graphql_errors_are_errors() {
        let json = include_str!("../test_fixtures/graphql_error.json");
        let message = parse_inbox("github.com", json, &[])
            .expect_err("error")
            .to_string();
        assert!(
            message.contains("Could not resolve to a Repository"),
            "{message}"
        );
        assert!(parse_detail(&key(), json).is_err());
        let partial = r#"{"data":{"repository":null},"errors":[{"message":"boom"}]}"#;
        assert_eq!(
            parse_detail(&key(), partial)
                .expect_err("error")
                .to_string(),
            "boom"
        );
    }

    #[test]
    fn inbox_request_shape() {
        let tracked = vec!["PR_x".to_string()];
        let request = inbox_request("ghe.corp", "alice", &tracked);
        let expected = graphql_request("ghe.corp", "", serde_json::json!({}));
        assert_eq!(request.args, expected.args);
        let body: serde_json::Value =
            serde_json::from_str(&request.stdin.expect("stdin")).expect("json");
        assert_eq!(
            body["variables"],
            serde_json::json!({"login": "alice", "tracked": ["PR_x"]})
        );
        let query = body["query"].as_str().expect("query");
        assert!(query.contains("states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]"));
        assert!(query.contains("lastCommit: commits(last: 1)"));
        for search in ["review-requested:@me", "author:@me", "mentions:@me"] {
            assert!(
                query.contains(&format!("is:pr is:open archived:false {search}")),
                "{search}"
            );
        }
    }

    fn draft(path: &str, start_line: u32, end_line: u32, body: &str) -> crate::Draft {
        crate::Draft {
            id: None,
            key: key(),
            path: path.into(),
            side: Side::Right,
            start_line,
            end_line,
            commit: "head".into(),
            anchor_text: "    audit(user);".into(),
            body: body.into(),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
        }
    }

    fn stdin_json(request: &GhRequest) -> serde_json::Value {
        serde_json::from_str(request.stdin.as_deref().expect("stdin")).expect("json")
    }

    #[test]
    fn review_request_shape() {
        let commentable =
            std::collections::HashMap::from_iter([("src/upload.rs".to_string(), vec![40..=43])]);
        let mut left = draft("src/upload.rs", 10, 11, "Why drop this?");
        left.side = Side::Left;
        let mut outdated = draft("src/upload.rs", 41, 41, "Stale");
        outdated.outdated = true;
        let mut general = draft("src/upload.rs", 41, 41, "Still true");
        general.outdated = true;
        general.general = true;
        let drafts = vec![
            draft("src/upload.rs", 42, 42, "Retry forever?"),
            draft("src/upload.rs", 47, 47, "Unchanged here"),
            draft("src/audit.rs", 3, 3, "Audit this"),
            left,
            outdated,
            general,
            draft("src/upload.rs", 41, 41, "  "),
            crate::Draft {
                post_state: crate::PostState::Sending,
                ..draft("src/upload.rs", 41, 41, "Maybe posted")
            },
        ];
        let request = review_request(
            &key(),
            "head",
            ReviewEvent::RequestChanges,
            "Needs work.",
            &drafts,
            &commentable,
            &|path| path.ends_with(".rs").then(|| "rust".to_string()),
        );
        assert_eq!(
            request.args,
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/acme/web/pulls/482/reviews",
                "--input",
                "-"
            ]
        );
        let body = stdin_json(&request);
        assert_eq!(body["commit_id"], "head");
        assert_eq!(body["event"], "REQUEST_CHANGES");
        let unchanged = crate::quoted_body(&drafts[1], &key(), "head", Some("rust"));
        assert_eq!(
            body["comments"],
            serde_json::json!([
                {"path": "src/upload.rs", "body": "Retry forever?", "line": 42, "side": "RIGHT"},
                {"path": "src/upload.rs", "line": 43, "side": "RIGHT", "body": unchanged},
                {"path": "src/upload.rs", "body": "Why drop this?", "line": 11, "side": "LEFT",
                 "start_line": 10, "start_side": "LEFT"},
            ])
        );
        let review_body = body["body"].as_str().expect("body");
        assert!(review_body.starts_with("Needs work.\n\n### src/audit.rs\n\n"));
        assert!(review_body.contains("Audit this"));
        assert!(review_body.contains("Still true"));
        assert!(!review_body.contains("Stale"));
        assert!(!body.to_string().contains("Maybe posted"));
        assert_eq!(ReviewEvent::Comment.as_str(), "COMMENT");
        assert_eq!(ReviewEvent::Approve.as_str(), "APPROVE");
    }

    #[test]
    fn reply_resolve_and_viewed_requests() {
        let reply = reply_request(&key(), 1001, "Agreed.");
        assert_eq!(
            reply.args,
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/acme/web/pulls/482/comments/1001/replies",
                "--input",
                "-"
            ]
        );
        assert_eq!(stdin_json(&reply), serde_json::json!({"body": "Agreed."}));

        for (resolved, mutation) in [
            (true, "resolveReviewThread"),
            (false, "unresolveReviewThread"),
        ] {
            let request = resolve_request("ghe.corp", "PRRT_1", resolved);
            assert_eq!(
                request.args,
                graphql_request("ghe.corp", "", serde_json::json!({})).args
            );
            let body = stdin_json(&request);
            assert_eq!(body["variables"], serde_json::json!({"threadId": "PRRT_1"}));
            let query = body["query"].as_str().expect("query");
            assert!(
                query.contains(&format!("{mutation}(input: {{threadId: $threadId}})")),
                "{query}"
            );
        }

        for (viewed, mutation) in [(true, "markFileAsViewed"), (false, "unmarkFileAsViewed")] {
            let body = stdin_json(&viewed_request(
                "github.com",
                "PR_482",
                "src/a b.rs",
                viewed,
            ));
            assert_eq!(
                body["variables"],
                serde_json::json!({"pullRequestId": "PR_482", "path": "src/a b.rs"})
            );
            let query = body["query"].as_str().expect("query");
            assert!(
                query.contains(&format!(
                    "{mutation}(input: {{pullRequestId: $pullRequestId, path: $path}})"
                )),
                "{query}"
            );
        }
    }

    #[test]
    fn mutation_and_reply_responses() {
        assert!(parse_mutation(r#"{"data":{"resolveReviewThread":{}}}"#).is_ok());
        assert_eq!(
            parse_mutation(r#"{"data":null,"errors":[{"message":"no access"}]}"#)
                .expect_err("error")
                .to_string(),
            "no access"
        );
        let comment = parse_reply(
            r#"{"id":2001,"user":{"login":"carol"},"body":"Agreed.","created_at":"2026-10-02T09:00:00Z"}"#,
        )
        .expect("reply");
        assert_eq!(
            comment,
            ThreadComment {
                database_id: 2001,
                author: "carol".into(),
                body: "Agreed.".into(),
                created_at: "2026-10-02T09:00:00Z".into(),
            }
        );
    }

    #[test]
    fn detail_request_shape() {
        let body: serde_json::Value =
            serde_json::from_str(&detail_request(&key(), "alice").stdin.expect("stdin"))
                .expect("json");
        assert_eq!(
            body["variables"],
            serde_json::json!({"owner": "acme", "name": "web", "number": 482, "login": "alice"})
        );
    }
}
