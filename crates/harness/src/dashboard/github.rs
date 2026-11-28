//! Read-only GitHub queries through `gh`, with the token passed in the environment and kept out
//! of every message.
//!
//! The token reaches `gh` only as `GH_TOKEN` in the child's environment, never in its argv.
//! Every error message has the token removed before it leaves this module, and `Debug` never
//! shows it.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use super::command::{self, Environment, MAX_OUTPUT_BYTES, RunError};
use super::queue::IssueNode;
use super::text::{head_chars, split_lines};

const ISSUES_QUERY: &str = "
query($owner: String!, $name: String!, $label: String!, $after: String) {
  repository(owner: $owner, name: $name) {
    issues(first: 100, after: $after, states: OPEN, labels: [$label], orderBy: {field: UPDATED_AT, direction: DESC}) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes {
        number title url updatedAt body
        labels(first: 30) { nodes { name } }
        comments(last: 1) { totalCount nodes { createdAt } }
      }
    }
  }
}
";
const MERGED_QUERY: &str = "
query($search: String!) {
  search(query: $search, type: ISSUE, first: 50) {
    issueCount
    nodes { ... on PullRequest { number title url mergedAt baseRefName author { login } } }
  }
}
";
const CLOSED_QUERY: &str = "
query($search: String!) {
  search(query: $search, type: ISSUE, first: 50) {
    issueCount
    nodes { ... on Issue { number title url closedAt } }
  }
}
";
const MAX_ISSUE_PAGES: usize = 5;
const STATES_PER_QUERY: usize = 60;

/// A `gh` call failed; the message is already redacted.
#[derive(Debug)]
pub struct GitHubError(pub String);

impl std::fmt::Display for GitHubError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GitHubError: {}", self.0)
    }
}

/// An issue that closed lately.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClosedNode {
    pub number: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "closedAt", default)]
    pub closed_at: Option<String>,
}

/// Runs read-only GraphQL and REST queries for one repository.
pub struct GitHubClient {
    pub repo: String,
    owner: String,
    name: String,
    gh_binary: String,
    timeout: Duration,
    token: Option<String>,
    secrets: Vec<String>,
}

/// The GitHub tokens of the dashboard's own environment, which an error can echo too.
pub fn environment_secrets() -> Vec<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
        .collect()
}

/// The text with each secret value replaced, then every secret-looking value masked.
pub fn scrub(secrets: &[String], text: &str) -> String {
    let mut text = text.to_owned();
    for secret in secrets {
        text = text.replace(secret.as_str(), "[redacted]");
    }
    super::redact::redact(&text)
}

impl std::fmt::Debug for GitHubClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "GitHubClient(repo={:?}, token={})",
            self.repo,
            if self.token.is_some() { "set" } else { "unset" }
        )
    }
}

impl GitHubClient {
    /// `repo` is `owner/name`; a missing token lets `gh` use its own login.
    pub fn new(repo: &str, token: Option<&str>, gh_binary: &str) -> Result<Self, String> {
        let Some((owner, name)) = repo
            .split_once('/')
            .filter(|(owner, name)| !owner.is_empty() && !name.is_empty())
        else {
            return Err("GH_REPO must be owner/name".to_owned());
        };
        Ok(Self {
            repo: repo.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
            gh_binary: gh_binary.to_owned(),
            timeout: Duration::from_secs(30),
            token: token.filter(|token| !token.is_empty()).map(str::to_owned),
            secrets: token
                .filter(|token| !token.is_empty())
                .map(str::to_owned)
                .into_iter()
                .chain(environment_secrets())
                .collect(),
        })
    }

    /// The text with the token, the environment's GitHub tokens and every secret-looking value
    /// masked.
    pub fn redact(&self, text: &str) -> String {
        scrub(&self.secrets, text)
    }

    /// The variables set on top of the dashboard's environment for a `gh` (or gh-calling) child:
    /// non-interactive `gh` settings, the token and `extra`.
    pub fn child_environment(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut environment: Vec<(String, String)> = [
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("NO_COLOR", "1"),
        ]
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
        if let Some(token) = &self.token {
            environment.push(("GH_TOKEN".to_owned(), token.clone()));
        }
        environment.extend(
            extra
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        );
        environment
    }

    /// The repository's open issues carrying a label, most recently updated first.
    pub fn fetch_open_issues(&self, label: &str) -> Result<Vec<IssueNode>, GitHubError> {
        let mut issues = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_ISSUE_PAGES {
            let mut variables = vec![
                ("owner", self.owner.clone()),
                ("name", self.name.clone()),
                ("label", label.to_owned()),
            ];
            if let Some(cursor) = &cursor {
                variables.push(("after", cursor.clone()));
            }
            let data = self.graphql(ISSUES_QUERY, &variables, false)?;
            let connection = &data["repository"]["issues"];
            let nodes = connection["nodes"]
                .as_array()
                .ok_or_else(|| shape("repository.issues.nodes"))?;
            for node in nodes {
                issues.push(serde_json::from_value(node.clone()).map_err(|error| {
                    GitHubError(format!("an issue node does not read: {error}"))
                })?);
            }
            let page = &connection["pageInfo"];
            if page["hasNextPage"].as_bool() != Some(true) {
                break;
            }
            cursor = page["endCursor"].as_str().map(str::to_owned);
        }
        Ok(issues)
    }

    /// The state (OPEN, CLOSED or MERGED) of issues or PRs by number; a number GitHub does not
    /// resolve is left out.
    pub fn fetch_states(&self, numbers: &[i64]) -> Result<BTreeMap<i64, String>, GitHubError> {
        let mut states = BTreeMap::new();
        for chunk in numbers.chunks(STATES_PER_QUERY) {
            let fields: Vec<String> = chunk
                .iter()
                .map(|number| {
                    format!("n{number}: issueOrPullRequest(number: {number}) {{ ... on Issue {{ state }} ... on PullRequest {{ state }} }}")
                })
                .collect();
            let query = format!(
                "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {} }} }}",
                fields.join(" ")
            );
            let data = self.graphql(
                &query,
                &[("owner", self.owner.clone()), ("name", self.name.clone())],
                true,
            )?;
            for number in chunk {
                if let Some(state) = data["repository"][format!("n{number}")]["state"].as_str() {
                    states.insert(*number, state.to_owned());
                }
            }
        }
        Ok(states)
    }

    /// PRs merged since a time, newest merge first.
    pub fn fetch_merged_pulls(&self, since_iso: &str) -> Result<Vec<Value>, GitHubError> {
        let search = format!("repo:{} is:pr is:merged merged:>={since_iso}", self.repo);
        let data = self.graphql(MERGED_QUERY, &[("search", search)], false)?;
        let mut pulls = numbered_nodes(&data)?;
        pulls.sort_by(|a, b| text_of(b, "mergedAt").cmp(text_of(a, "mergedAt")));
        Ok(pulls)
    }

    /// Issues carrying a label that closed since a time, latest close first.
    pub fn fetch_closed_issues(
        &self,
        label: &str,
        since_iso: &str,
    ) -> Result<Vec<ClosedNode>, GitHubError> {
        let search = format!(
            "repo:{} is:issue is:closed label:\"{label}\" closed:>={since_iso}",
            self.repo
        );
        let data = self.graphql(CLOSED_QUERY, &[("search", search)], false)?;
        let mut issues = numbered_nodes(&data)?
            .into_iter()
            .map(|node| {
                serde_json::from_value::<ClosedNode>(node)
                    .map_err(|error| GitHubError(format!("a closed issue does not read: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        issues.sort_by(|a, b| {
            b.closed_at
                .as_deref()
                .unwrap_or_default()
                .cmp(a.closed_at.as_deref().unwrap_or_default())
        });
        Ok(issues)
    }

    /// The issue whose claim comment names a tick pid (`... pid <N> claiming` or `resuming`).
    pub fn find_issue_for_pid(&self, pid: i64) -> Result<Option<i64>, GitHubError> {
        let query = format!("q=repo:{} \"pid {pid}\" in:comments", self.repo);
        let output = self.run(
            &[
                "api",
                "-X",
                "GET",
                "search/issues",
                "-f",
                &query,
                "-f",
                "per_page=5",
            ],
            false,
        )?;
        let document: Value = serde_json::from_str(&output)
            .map_err(|error| GitHubError(format!("gh returned non-JSON output ({error})")))?;
        for item in document["items"].as_array().into_iter().flatten() {
            if item.get("pull_request").is_none() {
                return item["number"]
                    .as_i64()
                    .map(Some)
                    .ok_or_else(|| shape("items.number"));
            }
        }
        Ok(None)
    }

    fn graphql(
        &self,
        query: &str,
        variables: &[(&str, String)],
        allow_partial: bool,
    ) -> Result<Value, GitHubError> {
        let mut arguments = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
        ];
        for (key, value) in variables {
            arguments.push("-f".to_owned());
            arguments.push(format!("{key}={value}"));
        }
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = self.run(&arguments, allow_partial)?;
        let document: Value = serde_json::from_str(&output)
            .map_err(|error| GitHubError(format!("gh returned non-JSON output ({error})")))?;
        let has_data = document
            .get("data")
            .is_some_and(|data| !data.is_null() && data != &Value::Object(serde_json::Map::new()));
        if let Some(errors) = document
            .get("errors")
            .and_then(Value::as_array)
            .filter(|errors| !errors.is_empty())
            && !(allow_partial && has_data)
        {
            let messages: Vec<String> = errors
                .iter()
                .map(|entry| match entry.get("message") {
                    Some(Value::String(message)) => message.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                })
                .collect();
            // Masked before it is cut, so a cut cannot leave part of a secret behind.
            let messages = self.redact(&messages.join("; "));
            return Err(GitHubError(format!(
                "GraphQL errors: {}",
                head_chars(&messages, 300)
            )));
        }
        Ok(document
            .get("data")
            .filter(|data| !data.is_null())
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new())))
    }

    fn run(
        &self,
        arguments: &[&str],
        accept_failure_with_output: bool,
    ) -> Result<String, GitHubError> {
        let mut argv = vec![self.gh_binary.clone()];
        argv.extend(arguments.iter().map(|argument| (*argument).to_owned()));
        let output = command::run(
            &argv,
            Environment::Inherit(self.child_environment(&[])),
            self.timeout,
            MAX_OUTPUT_BYTES,
        )
        .map_err(|error| match error {
            RunError::Spawn { error, .. } if error.kind() == std::io::ErrorKind::NotFound => {
                GitHubError(format!("{} not found", self.gh_binary))
            }
            RunError::Timeout { .. } => GitHubError(format!(
                "gh timed out after {:.0} s",
                self.timeout.as_secs_f64()
            )),
            other => GitHubError(self.redact(&other.to_string())),
        })?;
        if !output.success() && (!accept_failure_with_output || output.stdout.trim().is_empty()) {
            let detail = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            let lines = split_lines(detail.trim());
            let message = lines.last().map_or_else(
                || format!("exit {}", output.status),
                |line| head_chars(&self.redact(line), 300).to_owned(),
            );
            return Err(GitHubError(format!("gh exit {}: {message}", output.status)));
        }
        Ok(output.stdout)
    }
}

fn shape(field: &str) -> GitHubError {
    GitHubError(format!("gh output has no {field}"))
}

fn text_of<'v>(node: &'v Value, key: &str) -> &'v str {
    node.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The search result's nodes that carry a number: a node of another type reads as `{}`.
fn numbered_nodes(data: &Value) -> Result<Vec<Value>, GitHubError> {
    let nodes = data["search"]["nodes"]
        .as_array()
        .ok_or_else(|| shape("search.nodes"))?;
    Ok(nodes
        .iter()
        .filter(|node| {
            node.get("number")
                .is_some_and(|number| number.as_i64().is_some_and(|number| number != 0))
        })
        .cloned()
        .collect())
}
