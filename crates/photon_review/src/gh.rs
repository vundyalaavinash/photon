use std::{fmt, path::PathBuf, sync::Arc};

use futures::{AsyncWriteExt as _, future::BoxFuture};
use gpui::{App, Global};
use util::command::{Stdio, new_command};

#[derive(Clone, Debug, PartialEq)]
pub struct GhRequest {
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GhError {
    NotInstalled,
    LoggedOut { host: Option<String> },
    Failed { message: String },
}

impl fmt::Display for GhError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GhError::NotInstalled => write!(
                formatter,
                "GitHub CLI not found. Install it with \"brew install gh\", then refresh."
            ),
            GhError::LoggedOut { host: Some(host) } => write!(
                formatter,
                "Not logged in to {host}. Run \"gh auth login --hostname {host}\" in a terminal, then refresh."
            ),
            GhError::LoggedOut { host: None } => write!(
                formatter,
                "Not logged in to GitHub. Run \"gh auth login\" in a terminal, then refresh."
            ),
            GhError::Failed { message } => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for GhError {}

pub trait GhRunner: Send + Sync + 'static {
    fn run(&self, request: GhRequest) -> BoxFuture<'static, Result<String, GhError>>;
}

pub struct SystemGh {
    program: PathBuf,
}

impl SystemGh {
    pub fn new() -> Self {
        Self::with_program("gh")
    }

    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Default for SystemGh {
    fn default() -> Self {
        Self::new()
    }
}

const GH_ENVIRONMENT: [(&str, &str); 5] = [
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"),
    ("GH_SPINNER_DISABLED", "1"),
    ("NO_COLOR", "1"),
];

const LOGGED_OUT_EXIT_CODE: i32 = 4;

impl GhRunner for SystemGh {
    fn run(&self, request: GhRequest) -> BoxFuture<'static, Result<String, GhError>> {
        let program = self.program.clone();
        Box::pin(async move {
            let failed = |error: std::io::Error| GhError::Failed {
                message: error.to_string(),
            };
            let mut command = new_command(&program);
            command
                .args(&request.args)
                .envs(GH_ENVIRONMENT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    GhError::NotInstalled
                } else {
                    failed(error)
                }
            })?;
            if let Some(mut stdin) = child.stdin.take() {
                if let Some(input) = &request.stdin {
                    // A stub that exits without reading closes the pipe; the exit status
                    // below is the real outcome.
                    if let Err(error) = stdin.write_all(input.as_bytes()).await
                        && error.kind() != std::io::ErrorKind::BrokenPipe
                    {
                        return Err(failed(error));
                    }
                }
                drop(stdin);
            }
            let output = child.output().await.map_err(failed)?;
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if output.status.success() {
                return Ok(stdout);
            }
            if output.status.code() == Some(LOGGED_OUT_EXIT_CODE) {
                let host = request
                    .args
                    .iter()
                    .position(|argument| argument == "--hostname")
                    .and_then(|index| request.args.get(index + 1))
                    .cloned();
                return Err(GhError::LoggedOut { host });
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut parts = vec![stderr.trim().to_string()];
            if let Ok(body) = serde_json::from_str::<serde_json::Value>(&stdout) {
                collect_messages(&body, &mut parts);
            }
            parts.retain(|part| !part.is_empty());
            Err(GhError::Failed {
                message: parts.join(": "),
            })
        })
    }
}

fn collect_messages(value: &serde_json::Value, messages: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(message)) = map.get("message") {
                messages.push(message.clone());
            }
            if let Some(errors) = map.get("errors") {
                collect_messages(errors, messages);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_messages(item, messages);
            }
        }
        _ => {}
    }
}

struct GlobalGhRunner(Arc<dyn GhRunner>);

impl Global for GlobalGhRunner {}

pub fn gh_runner(cx: &App) -> Arc<dyn GhRunner> {
    match cx.try_global::<GlobalGhRunner>() {
        Some(global) => global.0.clone(),
        None => Arc::new(SystemGh::new()),
    }
}

pub fn set_gh_runner(runner: Arc<dyn GhRunner>, cx: &mut App) {
    cx.set_global(GlobalGhRunner(runner));
}

pub fn graphql_request(host: &str, query: &str, variables: serde_json::Value) -> GhRequest {
    GhRequest {
        args: ["api", "graphql", "--hostname", host, "--input", "-"]
            .map(String::from)
            .to_vec(),
        stdin: Some(serde_json::json!({ "query": query, "variables": variables }).to_string()),
    }
}

pub fn rest_request(
    host: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> GhRequest {
    let mut args: Vec<String> = ["api", "--hostname", host, "--method", method, path]
        .map(String::from)
        .to_vec();
    if body.is_some() {
        args.extend(["--input", "-"].map(String::from));
    }
    GhRequest {
        args,
        stdin: body.map(|body| body.to_string()),
    }
}

#[cfg(any(test, feature = "test-support"))]
type Matcher = Box<dyn Fn(&GhRequest) -> bool + Send + Sync>;

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct FakeGh {
    responses: parking_lot::Mutex<Vec<(Matcher, Result<String, GhError>)>>,
    requests: parking_lot::Mutex<Vec<GhRequest>>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeGh {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn respond(
        &self,
        matches: impl Fn(&GhRequest) -> bool + Send + Sync + 'static,
        result: Result<String, GhError>,
    ) {
        self.responses.lock().push((Box::new(matches), result));
    }

    pub fn requests(&self) -> Vec<GhRequest> {
        self.requests.lock().clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl GhRunner for FakeGh {
    fn run(&self, request: GhRequest) -> BoxFuture<'static, Result<String, GhError>> {
        let mut responses = self.responses.lock();
        let result = match responses.iter().position(|(matches, _)| matches(&request)) {
            Some(index) => responses.remove(index).1,
            None => Err(GhError::Failed {
                message: "FakeGh: unexpected request".to_string(),
            }),
        };
        self.requests.lock().push(request);
        Box::pin(std::future::ready(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn stub(dir: &tempfile::TempDir, script: &str) -> SystemGh {
        let path = dir.path().join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        SystemGh::with_program(path)
    }

    fn request(args: &[&str], stdin: Option<&str>) -> GhRequest {
        GhRequest {
            args: args.iter().map(|argument| argument.to_string()).collect(),
            stdin: stdin.map(String::from),
        }
    }

    #[gpui::test]
    async fn system_gh_passes_args_and_stdin(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().expect("tempdir");
        let gh = stub(&dir, "echo \"$@\"\ncat");
        let graphql = graphql_request("github.com", "query { a }", serde_json::json!({"n": 1}));
        let stdin = graphql.stdin.clone().expect("stdin");
        let output = gh.run(graphql).await.expect("run");
        assert_eq!(
            output,
            format!("api graphql --hostname github.com --input -\n{stdin}")
        );
    }

    #[gpui::test]
    async fn system_gh_sets_no_prompt_env(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().expect("tempdir");
        let gh = stub(
            &dir,
            "printf '%s%s' \"$GH_PROMPT_DISABLED\" \"$GH_NO_UPDATE_NOTIFIER\"",
        );
        let output = gh.run(request(&["x"], None)).await.expect("run");
        assert_eq!(output, "11");
    }

    #[gpui::test]
    async fn system_gh_exit_4_is_logged_out(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().expect("tempdir");
        let gh = stub(&dir, "exit 4");
        let error = gh
            .run(request(&["api", "--hostname", "ghe.corp", "x"], None))
            .await
            .expect_err("logged out");
        assert_eq!(
            error,
            GhError::LoggedOut {
                host: Some("ghe.corp".into())
            }
        );
        let error = gh
            .run(request(&["api"], None))
            .await
            .expect_err("logged out");
        assert_eq!(error, GhError::LoggedOut { host: None });
    }

    #[gpui::test]
    async fn system_gh_missing_binary_is_not_installed(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().expect("tempdir");
        let gh = SystemGh::with_program(dir.path().join("missing"));
        let error = gh.run(request(&["x"], None)).await.expect_err("missing");
        assert_eq!(error, GhError::NotInstalled);
    }

    #[gpui::test]
    async fn system_gh_failure_carries_github_message(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().expect("tempdir");
        let gh = stub(
            &dir,
            "echo 'gh: Validation Failed (HTTP 422)' >&2\n\
             echo '{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"line must be part of the diff\"}]}'\n\
             exit 1",
        );
        let GhError::Failed { message } = gh.run(request(&["x"], None)).await.expect_err("failed")
        else {
            panic!("expected Failed");
        };
        assert_eq!(
            message,
            "gh: Validation Failed (HTTP 422): Validation Failed: line must be part of the diff"
        );
    }

    #[test]
    fn request_builders() {
        let graphql = graphql_request("h", "q", serde_json::json!({"a": 1}));
        assert_eq!(
            graphql.args,
            ["api", "graphql", "--hostname", "h", "--input", "-"]
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&graphql.stdin.expect("stdin"))
                .expect("json"),
            serde_json::json!({"query": "q", "variables": {"a": 1}})
        );

        let get = rest_request("h", "GET", "repos/o/r", None);
        assert_eq!(
            get.args,
            ["api", "--hostname", "h", "--method", "GET", "repos/o/r"]
        );
        assert_eq!(get.stdin, None);

        let post = rest_request("h", "POST", "p", Some(serde_json::json!({"b": 2})));
        assert_eq!(
            post.args,
            [
                "api",
                "--hostname",
                "h",
                "--method",
                "POST",
                "p",
                "--input",
                "-"
            ]
        );
        assert_eq!(post.stdin.as_deref(), Some("{\"b\":2}"));
    }

    #[gpui::test]
    async fn fake_gh_records_and_matches() {
        let fake = FakeGh::new();
        fake.respond(|request| request.args[0] == "a", Ok("first".into()));
        fake.respond(|request| request.args[0] == "b", Ok("second".into()));
        assert_eq!(fake.run(request(&["b"], None)).await, Ok("second".into()));
        assert_eq!(fake.run(request(&["a"], None)).await, Ok("first".into()));
        assert_eq!(
            fake.run(request(&["a"], None)).await,
            Err(GhError::Failed {
                message: "FakeGh: unexpected request".into()
            })
        );
        let seen: Vec<String> = fake
            .requests()
            .iter()
            .map(|request| request.args[0].clone())
            .collect();
        assert_eq!(seen, ["b", "a", "a"]);
    }
}
