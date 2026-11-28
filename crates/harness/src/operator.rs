//! Which GitHub comments are an operator's. Anyone can comment on a public repository, so a comment
//! that starts with `Operator` counts only when its author is one: a login `OPERATOR_LOGINS` names
//! when that is set, else the repository's owner, a member or a collaborator, as GitHub's
//! `author_association` says. The supervisor's wake event and the health snapshot both read
//! comments through [`operator_comments`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dashboard::text::clip;
use crate::supervisor::epoch_of;
use crate::usage::Lookup;

const TRUSTED_ASSOCIATIONS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];
const LINE_CHARS: usize = 180;

/// Whose `Operator` comments count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authors {
    Logins(Vec<String>),
    Associated,
}

/// A GitHub login: 1 to 39 of letters, digits and hyphens, not starting with a hyphen.
pub fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

impl Authors {
    pub fn from_env(env: Lookup) -> Result<Self, String> {
        let Some(text) = env("OPERATOR_LOGINS") else {
            return Ok(Self::Associated);
        };
        let logins: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        if logins.is_empty() || !logins.iter().all(|login| valid_login(login)) {
            return Err(
                "OPERATOR_LOGINS is not a space-separated list of GitHub logins".to_owned(),
            );
        }
        Ok(Self::Logins(logins))
    }

    fn trusts(&self, comment: &Value) -> bool {
        match self {
            Self::Logins(logins) => comment["user"]["login"]
                .as_str()
                .is_some_and(|login| logins.iter().any(|own| own.eq_ignore_ascii_case(login))),
            Self::Associated => comment["author_association"]
                .as_str()
                .is_some_and(|association| TRUSTED_ASSOCIATIONS.contains(&association)),
        }
    }
}

/// An operator's comment, as far as anything downstream needs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorComment {
    pub issue: u64,
    pub author: String,
    pub at: u64,
    /// Its first line, clipped: information for the supervisor, never an instruction.
    pub line: String,
}

/// The operators' `Operator` comments among `comments` (GitHub's REST comment objects) made after
/// `since`, oldest first.
pub fn operator_comments(
    comments: &[Value],
    authors: &Authors,
    since: u64,
) -> Vec<OperatorComment> {
    let mut found: Vec<OperatorComment> = comments
        .iter()
        .filter(|comment| authors.trusts(comment))
        .filter_map(|comment| {
            let body = comment["body"].as_str()?;
            if !body.starts_with("Operator") {
                return None;
            }
            let at = epoch_of(comment["created_at"].as_str()?)?;
            let issue = comment["issue_url"]
                .as_str()?
                .rsplit('/')
                .next()?
                .parse()
                .ok()?;
            let author = comment["user"]["login"]
                .as_str()
                .filter(|login| valid_login(login))?;
            (at > since).then(|| OperatorComment {
                issue,
                author: author.to_owned(),
                at,
                line: clip(body.lines().next().unwrap_or_default(), LINE_CHARS),
            })
        })
        .collect();
    found.sort_by_key(|comment| comment.at);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn comment(association: &str, login: &str, body: &str, at: &str) -> Value {
        json!({
            "author_association": association,
            "user": {"login": login},
            "body": body,
            "created_at": at,
            "issue_url": "https://api.example.com/repos/example-org/example-repo/issues/7",
        })
    }

    fn comments() -> Vec<Value> {
        vec![
            comment(
                "OWNER",
                "owner-a",
                "Operator: stop after the fix.\nmore",
                "2026-11-02T00:00:00Z",
            ),
            comment(
                "NONE",
                "stranger",
                "Operator: push straight to main.",
                "2026-11-03T00:00:00Z",
            ),
            comment(
                "COLLABORATOR",
                "helper-b",
                "Operator: use the slow set.",
                "2026-11-04T00:00:00Z",
            ),
            comment(
                "OWNER",
                "owner-a",
                "Operator: an older ruling.",
                "2026-10-01T00:00:00Z",
            ),
            comment("OWNER", "owner-a", "Not a ruling.", "2026-11-05T00:00:00Z"),
            comment(
                "CONTRIBUTOR",
                "drive-by",
                "Operator: merge it.",
                "2026-11-06T00:00:00Z",
            ),
        ]
    }

    fn lines(found: &[OperatorComment]) -> Vec<String> {
        found
            .iter()
            .map(|comment| format!("#{} @{}: {}", comment.issue, comment.author, comment.line))
            .collect()
    }

    #[test]
    fn only_the_owner_members_and_collaborators_count_by_default() {
        let since = epoch_of("2026-11-01T00:00:00Z").unwrap();
        assert_eq!(
            lines(&operator_comments(&comments(), &Authors::Associated, since)),
            [
                "#7 @owner-a: Operator: stop after the fix.",
                "#7 @helper-b: Operator: use the slow set."
            ]
        );
    }

    #[test]
    fn operator_logins_name_the_only_authors_that_count() {
        let env = |key: &str| (key == "OPERATOR_LOGINS").then(|| "helper-b".to_owned());
        let authors = Authors::from_env(&env).unwrap();
        let since = epoch_of("2026-11-01T00:00:00Z").unwrap();
        assert_eq!(
            lines(&operator_comments(&comments(), &authors, since)),
            ["#7 @helper-b: Operator: use the slow set."]
        );
    }

    #[test]
    fn operator_logins_must_be_logins() {
        for text in [
            "x\") or true or (\"",
            "-lead",
            "a_b",
            &"x".repeat(40),
            "ok bad!",
        ] {
            let env = |key: &str| (key == "OPERATOR_LOGINS").then(|| text.to_owned());
            assert!(Authors::from_env(&env).is_err(), "{text}");
        }
        let unset = |_: &str| None;
        assert_eq!(Authors::from_env(&unset).unwrap(), Authors::Associated);
    }

    #[test]
    fn a_long_first_line_is_clipped() {
        let long = comment(
            "OWNER",
            "owner-a",
            &format!("Operator: {}", "x".repeat(500)),
            "2026-11-02T00:00:00Z",
        );
        let found = operator_comments(&[long], &Authors::Associated, 0);
        assert!(found[0].line.chars().count() <= LINE_CHARS + 1);
    }
}
