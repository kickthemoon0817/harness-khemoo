//! Groups the open issues the way the harness sees them.
//!
//! Sorts open issues into running, claimable, held, operator, signoff and other. The claimable
//! rule is the queue count's. An issue is a candidate when it carries the queue label, no live
//! local claim holds it, it is not parked for sign-off or for the operator, and every `Depends on`
//! issue is closed; a wip issue whose claimant is gone is a candidate again. The queue count also
//! reads each wip issue's claim comments for a live claimant pid; here a live claimant is a live
//! tick holding that issue's claim file, which the tick takes before it comments.
//!
//! The first-before-low gate then holds candidates back. A low issue counts only when no other
//! candidate counts and no first issue is claimable anywhere; a lane issue without the first label
//! counts only while no first issue is claimable anywhere. "Anywhere" includes first issues
//! outside the queue label, judged by the same candidate rule without it. A held-back candidate is
//! listed in `held` with the reason.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::text::is_word_char;

/// The label names the harness uses; the defaults are the launcher's and the queue count's. An
/// empty lane label turns the model lane off.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct QueueLabels {
    pub all_issues: String,
    pub queue: String,
    pub wip: String,
    pub signoff: String,
    pub operator: String,
    pub ready: String,
    pub lane: String,
    pub first: String,
    pub low: String,
}

impl Default for QueueLabels {
    fn default() -> Self {
        Self {
            all_issues: "ai".to_owned(),
            queue: "ai".to_owned(),
            wip: "ai:wip".to_owned(),
            signoff: "ai:signoff".to_owned(),
            operator: "ai:operator".to_owned(),
            ready: "ai:ready".to_owned(),
            lane: String::new(),
            first: "ai:first".to_owned(),
            low: "ai:low".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Named {
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LabelConnection {
    #[serde(default)]
    pub nodes: Vec<Named>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommentNode {
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommentConnection {
    #[serde(rename = "totalCount", default)]
    pub total_count: Option<i64>,
    #[serde(default)]
    pub nodes: Option<Vec<CommentNode>>,
}

/// An open issue as the GitHub GraphQL query returns it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IssueNode {
    pub number: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Option<LabelConnection>,
    #[serde(default)]
    pub comments: Option<CommentConnection>,
}

/// One open issue, its group and why it is there.
#[derive(Clone, Debug, Serialize)]
pub struct QueueIssue {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub labels: Vec<String>,
    pub updated_at: Option<String>,
    pub last_comment_at: Option<String>,
    pub comment_count: i64,
    pub depends_on: Vec<i64>,
    pub open_dependencies: Vec<i64>,
    pub group: String,
    pub reason: String,
    pub tick_pid: Option<i64>,
}

impl QueueIssue {
    fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|name| name == label)
    }
}

/// The groups in display order.
pub const GROUP_ORDER: [&str; 6] = [
    "running",
    "claimable",
    "held",
    "operator",
    "signoff",
    "other",
];

pub type Groups = BTreeMap<String, Vec<QueueIssue>>;

static DEPENDS_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Depends on[^:\n]*:\s*(.*)").expect("a constant pattern compiles")
});
static DEPENDENCY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#([0-9]+)").expect("a constant pattern compiles"));

/// Every issue number on the `Depends on ...: #a, #b` lines of an issue body, as the queue count
/// reads them: in order, repeats kept.
pub fn parse_dependencies(body: &str) -> Vec<i64> {
    DEPENDS_LINE
        .captures_iter(body)
        .filter_map(|line| line.get(1))
        .flat_map(|line| {
            let line = line.as_str();
            // `#N` ends at a word boundary: digits of another script, letters and `_` continue
            // the word, a combining mark or a joiner does not.
            DEPENDENCY
                .captures_iter(line)
                .filter_map(|found| {
                    let digits = found.get(1)?;
                    let bounded = !line[digits.end()..]
                        .chars()
                        .next()
                        .is_some_and(is_word_char);
                    (digits.len() <= 9 && bounded).then(|| digits.as_str().parse().ok())?
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub struct IssueQueue {
    pub labels: QueueLabels,
}

impl IssueQueue {
    /// Groups the issues. `dependency_states` holds the state (OPEN, CLOSED, MERGED) of
    /// dependencies outside the open list; `first_only_issues` are open first issues without the
    /// all-issues label, not listed, only weighed by the gate.
    pub fn classify(
        &self,
        raw_issues: &[IssueNode],
        dependency_states: &BTreeMap<i64, String>,
        live_claim_pid_by_issue: &HashMap<i64, i64>,
        tick_pid_by_issue: &HashMap<i64, i64>,
        first_only_issues: &[IssueNode],
    ) -> Groups {
        let open_numbers: HashSet<i64> = raw_issues.iter().map(|issue| issue.number).collect();
        let mut groups: Groups = GROUP_ORDER
            .iter()
            .map(|name| ((*name).to_owned(), Vec::new()))
            .collect();
        let mut first_claimable_outside_queue = false;
        let listed = raw_issues.iter().map(|raw| (raw, true));
        let weighed = first_only_issues.iter().map(|raw| (raw, false));
        for (raw_issue, is_listed) in listed.chain(weighed) {
            let issue = self.issue_of(
                raw_issue,
                &open_numbers,
                dependency_states,
                live_claim_pid_by_issue,
                tick_pid_by_issue,
            );
            if issue.has(&self.labels.first)
                && !issue.has(&self.labels.queue)
                && self.is_candidate_without_queue_label(
                    &issue,
                    live_claim_pid_by_issue,
                    tick_pid_by_issue,
                )
            {
                first_claimable_outside_queue = true;
            }
            if is_listed && let Some(group) = groups.get_mut(&issue.group) {
                group.push(issue);
            }
        }
        let candidates = groups.remove("claimable").unwrap_or_default();
        let (mut claimable, held) =
            self.apply_first_gate(candidates, first_claimable_outside_queue);
        claimable.sort_by_key(|issue| {
            (
                !issue.has(&self.labels.first),
                issue.has(&self.labels.low),
                issue.number,
            )
        });
        groups.insert("claimable".to_owned(), claimable);
        groups.insert("held".to_owned(), held);
        for name in ["running", "held", "operator", "signoff", "other"] {
            if let Some(group) = groups.get_mut(name) {
                group.sort_by(|a, b| sort_key(b).cmp(sort_key(a)));
            }
        }
        groups
    }

    /// Splits the candidates into those the queue count counts and those its first-before-low gate
    /// holds back, each held one with its reason.
    fn apply_first_gate(
        &self,
        candidates: Vec<QueueIssue>,
        first_claimable_outside_queue: bool,
    ) -> (Vec<QueueIssue>, Vec<QueueIssue>) {
        let labels = &self.labels;
        let is_new_lane_issue = |issue: &QueueIssue| {
            !labels.lane.is_empty() && issue.has(&labels.lane) && !issue.has(&labels.first)
        };
        let gated = candidates
            .iter()
            .any(|issue| issue.has(&labels.low) || is_new_lane_issue(issue));
        let first_waiting = gated
            && (candidates.iter().any(|issue| issue.has(&labels.first))
                || (labels.queue != labels.first && first_claimable_outside_queue));
        let mut kept: Vec<i64> = candidates
            .iter()
            .filter(|issue| {
                !(issue.has(&labels.low) || (is_new_lane_issue(issue) && first_waiting))
            })
            .map(|issue| issue.number)
            .collect();
        if kept.is_empty() && !first_waiting {
            kept = candidates
                .iter()
                .filter(|issue| issue.has(&labels.low))
                .map(|issue| issue.number)
                .collect();
        }
        let (counted, held): (Vec<QueueIssue>, Vec<QueueIssue>) = candidates
            .into_iter()
            .partition(|issue| kept.contains(&issue.number));
        let held = held
            .into_iter()
            .map(|mut issue| {
                issue.reason = if issue.has(&labels.low) {
                    if first_waiting {
                        format!(
                            "held: {} waits while a {} issue is claimable",
                            labels.low, labels.first
                        )
                    } else {
                        format!("held: {} waits while other work is claimable", labels.low)
                    }
                } else {
                    format!(
                        "held: a new {} issue waits while a {} issue is claimable",
                        labels.lane, labels.first
                    )
                };
                "held".clone_into(&mut issue.group);
                issue
            })
            .collect();
        (counted, held)
    }

    fn issue_of(
        &self,
        raw: &IssueNode,
        open_numbers: &HashSet<i64>,
        dependency_states: &BTreeMap<i64, String>,
        live_claim_pid_by_issue: &HashMap<i64, i64>,
        tick_pid_by_issue: &HashMap<i64, i64>,
    ) -> QueueIssue {
        let labels: Vec<String> = raw
            .labels
            .iter()
            .flat_map(|connection| connection.nodes.iter().map(|label| label.name.clone()))
            .collect();
        let depends_on = parse_dependencies(raw.body.as_deref().unwrap_or_default());
        let mut open_dependencies = Vec::new();
        for dependency in &depends_on {
            let state = dependency_states
                .get(dependency)
                .map_or("OPEN", String::as_str);
            if !open_dependencies.contains(dependency)
                && (open_numbers.contains(dependency) || !matches!(state, "CLOSED" | "MERGED"))
            {
                open_dependencies.push(*dependency);
            }
        }
        let (group, reason) = self.group_of(
            raw.number,
            &labels,
            &open_dependencies,
            live_claim_pid_by_issue,
            tick_pid_by_issue,
        );
        let comments = raw.comments.clone().unwrap_or_default();
        QueueIssue {
            number: raw.number,
            title: raw.title.clone().unwrap_or_default(),
            url: raw.url.clone().unwrap_or_default(),
            labels,
            updated_at: raw.updated_at.clone(),
            last_comment_at: comments
                .nodes
                .as_ref()
                .and_then(|nodes| nodes.last())
                .and_then(|node| node.created_at.clone()),
            comment_count: comments.total_count.unwrap_or_default(),
            depends_on,
            open_dependencies,
            group,
            reason,
            tick_pid: tick_pid_by_issue.get(&raw.number).copied(),
        }
    }

    /// The candidate rule but for the queue label, as the queue count applies it to first issues
    /// outside it.
    fn is_candidate_without_queue_label(
        &self,
        issue: &QueueIssue,
        live_claim_pid_by_issue: &HashMap<i64, i64>,
        tick_pid_by_issue: &HashMap<i64, i64>,
    ) -> bool {
        !tick_pid_by_issue.contains_key(&issue.number)
            && !live_claim_pid_by_issue.contains_key(&issue.number)
            && !issue.has(&self.labels.signoff)
            && !issue.has(&self.labels.operator)
            && issue.open_dependencies.is_empty()
    }

    fn group_of(
        &self,
        number: i64,
        labels: &[String],
        open_dependencies: &[i64],
        live_claim_pid_by_issue: &HashMap<i64, i64>,
        tick_pid_by_issue: &HashMap<i64, i64>,
    ) -> (String, String) {
        let has = |label: &str| labels.iter().any(|name| name == label);
        let (group, reason) = if let Some(pid) = tick_pid_by_issue.get(&number) {
            ("running", format!("tick pid {pid}"))
        } else if let Some(pid) = live_claim_pid_by_issue.get(&number) {
            ("other", format!("claimed by live pid {pid}"))
        } else if has(&self.labels.operator) {
            ("operator", "waits on the operator".to_owned())
        } else if has(&self.labels.signoff) {
            ("signoff", "waits for sign-off".to_owned())
        } else if !has(&self.labels.queue) {
            ("other", format!("not labelled {}", self.labels.queue))
        } else if !open_dependencies.is_empty() {
            let named: Vec<String> = open_dependencies
                .iter()
                .map(|dependency| format!("#{dependency}"))
                .collect();
            ("other", format!("waits on {}", named.join(", ")))
        } else if has(&self.labels.wip) {
            ("claimable", "resumable: no live claimant".to_owned())
        } else {
            ("claimable", "ready to claim".to_owned())
        };
        (group.to_owned(), reason)
    }
}

fn sort_key(issue: &QueueIssue) -> &str {
    issue
        .last_comment_at
        .as_deref()
        .or(issue.updated_at.as_deref())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::fixture_text;
    use serde_json::json;

    pub fn labels() -> QueueLabels {
        QueueLabels {
            queue: "ai:plan".to_owned(),
            lane: "ai:lane".to_owned(),
            ..QueueLabels::default()
        }
    }

    fn numbers(groups: &Groups, name: &str) -> Vec<i64> {
        groups[name].iter().map(|issue| issue.number).collect()
    }

    fn sorted(mut values: Vec<i64>) -> Vec<i64> {
        values.sort_unstable();
        values
    }

    #[test]
    fn groups() {
        let issues: Vec<IssueNode> =
            serde_json::from_str(&fixture_text("issues.json")).expect("fixture");
        let states: BTreeMap<i64, String> = [(2380, "CLOSED"), (2499, "MERGED"), (1229, "OPEN")]
            .map(|(number, state)| (number, state.to_owned()))
            .into();
        let groups = IssueQueue { labels: labels() }.classify(
            &issues,
            &states,
            &[(2501, 4242)].into(),
            &[(2464, 18907)].into(),
            &[],
        );
        assert_eq!(numbers(&groups, "running"), [2464]);
        assert_eq!(sorted(numbers(&groups, "claimable")), [2490, 2495, 2502]);
        assert_eq!(numbers(&groups, "claimable")[0], 2495);
        assert_eq!(sorted(numbers(&groups, "held")), [2503, 2504]);
        assert_eq!(sorted(numbers(&groups, "operator")), [2063, 2467]);
        assert_eq!(numbers(&groups, "signoff"), [2200]);
        assert_eq!(sorted(numbers(&groups, "other")), [1240, 2388, 2501]);
        let reasons: HashMap<i64, &str> = groups
            .values()
            .flatten()
            .map(|issue| (issue.number, issue.reason.as_str()))
            .collect();
        assert_eq!(reasons[&2388], "waits on #2063");
        assert_eq!(reasons[&1240], "not labelled ai:plan");
        assert_eq!(reasons[&2501], "claimed by live pid 4242");
        assert_eq!(reasons[&2490], "resumable: no live claimant");
        assert_eq!(reasons[&2464], "tick pid 18907");
        assert_eq!(
            reasons[&2503],
            "held: ai:low waits while a ai:first issue is claimable"
        );
        assert_eq!(
            reasons[&2504],
            "held: a new ai:lane issue waits while a ai:first issue is claimable"
        );
        let last_comment: HashMap<i64, Option<&str>> = groups
            .values()
            .flatten()
            .map(|issue| (issue.number, issue.last_comment_at.as_deref()))
            .collect();
        assert_eq!(last_comment[&2464], Some("2026-11-26T01:27:15Z"));
        assert_eq!(last_comment[&2495], None);
    }

    fn node(value: serde_json::Value) -> IssueNode {
        serde_json::from_value(value).expect("an issue node")
    }

    fn issue(number: i64, extra: &[&str], body: &str) -> IssueNode {
        let names: Vec<serde_json::Value> = ["ai", "ai:plan"]
            .iter()
            .chain(extra)
            .map(|name| json!({"name": name}))
            .collect();
        node(
            json!({"number": number, "title": format!("t{number}"), "body": body, "labels": {"nodes": names}}),
        )
    }

    fn split(issues: &[IssueNode], first_only: &[IssueNode]) -> (Vec<i64>, Vec<i64>) {
        let groups = IssueQueue { labels: labels() }.classify(
            issues,
            &BTreeMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            first_only,
        );
        (
            sorted(numbers(&groups, "claimable")),
            sorted(numbers(&groups, "held")),
        )
    }

    #[test]
    fn first_gate() {
        assert_eq!(
            split(&[issue(1, &[], ""), issue(2, &["ai:low"], "")], &[]),
            (vec![1], vec![2])
        );
        assert_eq!(
            split(&[issue(2, &["ai:low"], ""), issue(3, &["ai:low"], "")], &[]),
            (vec![2, 3], vec![])
        );
        assert_eq!(
            split(
                &[issue(4, &["ai:lane"], ""), issue(2, &["ai:low"], "")],
                &[]
            ),
            (vec![4], vec![2])
        );
        assert_eq!(
            split(
                &[
                    issue(4, &["ai:lane"], ""),
                    issue(5, &["ai:lane", "ai:first"], "")
                ],
                &[]
            ),
            (vec![5], vec![4])
        );
        let outside_first = node(
            json!({"number": 9, "title": "t9", "body": "", "labels": {"nodes": [{"name": "ai:first"}]}}),
        );
        assert_eq!(
            split(
                &[issue(4, &["ai:lane"], ""), issue(2, &["ai:low"], "")],
                std::slice::from_ref(&outside_first)
            ),
            (vec![], vec![2, 4])
        );
        assert_eq!(
            split(
                &[issue(4, &["ai:lane"], "")],
                std::slice::from_ref(&outside_first)
            ),
            (vec![], vec![4])
        );
        let parked_first = node(json!({"number": 9, "title": "t9", "body": "",
            "labels": {"nodes": [{"name": "ai:first"}, {"name": "ai:operator"}]}}));
        assert_eq!(
            split(&[issue(4, &["ai:lane"], "")], &[parked_first]),
            (vec![4], vec![])
        );
        let listed_outside_queue = node(json!({"number": 8, "title": "t8", "body": "",
            "labels": {"nodes": [{"name": "ai"}, {"name": "ai:first"}]}}));
        assert_eq!(
            split(&[issue(4, &["ai:lane"], ""), listed_outside_queue], &[]),
            (vec![], vec![4])
        );
        assert_eq!(
            split(
                &[issue(2, &["ai:low"], ""), issue(1, &[], "Depends on: #77")],
                &[]
            ),
            (vec![2], vec![])
        );
    }

    #[test]
    fn unknown_dependency_counts_as_open() {
        let issues = [node(
            json!({"number": 1, "title": "t", "body": "Depends on: #77", "labels": {"nodes": [{"name": "ai"}]}}),
        )];
        let groups = IssueQueue {
            labels: QueueLabels::default(),
        }
        .classify(
            &issues,
            &BTreeMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &[],
        );
        assert_eq!(groups["other"][0].reason, "waits on #77");
    }

    #[test]
    fn parse_dependencies_reads_every_line() {
        assert_eq!(
            parse_dependencies("Depends on: #1, #2\ntext\nDepends on (later): #3"),
            [1, 2, 3]
        );
        assert!(parse_dependencies(&format!("Depends on: #{}", "9".repeat(5000))).is_empty());
    }
}
