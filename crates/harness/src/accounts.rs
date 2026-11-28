//! `harness accounts`: the agent CLI accounts under `AGENT_ACCOUNTS_DIR`, whether each is signed in,
//! and the ticks its usage allows.
//!
//! An account is a folder the owner signed in to once, by hand, with the agent's own CLI:
//! `<root>/claude/claude-<name>` is run as `CLAUDE_CONFIG_DIR`, `<root>/codex/codex-<name>` as
//! `CODEX_HOME` and `<root>/gemini/gemini-<name>` as `GEMINI_CLI_HOME`. The harness never creates
//! one, signs in or reads a credential: it names the folder a tick runs with, and asks the agent's
//! CLI whether that folder is signed in. Of the CLI's answer only the fields [`SignIn`] holds are
//! kept, so no email, organization or token reaches a file, a log, the dashboard or the supervisor.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dashboard::command::{self, Environment};
use crate::dashboard::files;
use crate::supervisor::{epoch_of, now_s};
use crate::usage::{self, AgentUsage, Lookup};

/// The agents an account can belong to, each with the variable that points its CLI at a folder.
const AGENTS: [(&str, &str); 3] = [
    ("claude", "CLAUDE_CONFIG_DIR"),
    ("codex", "CODEX_HOME"),
    ("gemini", "GEMINI_CLI_HOME"),
];
/// How long a sign-in check holds: the launcher fires every couple of minutes, and no firing should
/// wait on the agents' CLIs.
pub const STATUS_TTL_S: u64 = 300;
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);
const STATUS_OUTPUT: usize = 64 * 1024;
const STATUS_FILE: u64 = 4096;
/// How many entries of one agent's folder are looked at.
const DIR_LIMIT: usize = 1000;

#[derive(clap::Args)]
#[command(
    args_conflicts_with_subcommands = true,
    after_help = "\
With no subcommand, prints each account folder under AGENT_ACCOUNTS_DIR: whether it is signed in and
how, when the sign-in ends where the CLI says, and its usage reading. A sign-in checked more than
five minutes ago is checked again first. Reads AGENT_ACCOUNTS_DIR, AGENT_BIN_<agent>, HARNESS_STATE,
MAX_SLOTS, TICK_WIDTH and the USAGE_* keys docs/INSTANCE.md gives from the environment."
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<Sub>,
    /// Print a JSON document instead of lines.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Print `VARIABLE=folder` for each agent@account, the setting its tick runs with; exit 1, naming
    /// each, when any folder is missing or refused.
    Env {
        #[arg(required = true)]
        accounts: Vec<String>,
    },
}

pub fn run(args: &Args) -> ExitCode {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    match &args.command {
        Some(Sub::Env { accounts }) => env_command(&env, accounts),
        None => match report(&env, args.json) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("harness accounts: {message}");
                ExitCode::from(2)
            }
        },
    }
}

/// The variable that points `agent`'s CLI at an account folder.
pub fn variable(agent: &str) -> Option<&'static str> {
    AGENTS
        .iter()
        .find(|(name, _)| *name == agent)
        .map(|(_, variable)| *variable)
}

/// An account name: 1 to 32 of a-z, 0-9 and `-`, starting with a letter or a digit.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// An account name as a setting's name carries it: `-` cannot appear there, so it reads `_`.
pub fn key_name(name: &str) -> String {
    name.replace('-', "_")
}

/// An account folder that passed [`Account::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub agent: String,
    pub name: String,
    pub dir: PathBuf,
}

impl Account {
    /// `<AGENT_ACCOUNTS_DIR>/<agent>/<agent>-<name>`, refused unless both folders below the root
    /// are real directories, not symlinks, and all three are this user's and not writable by every
    /// user: whoever could write there would choose the login a tick runs with. The root itself may
    /// be a symlink.
    pub fn resolve(env: Lookup, agent: &str, name: &str) -> Result<Self, String> {
        if variable(agent).is_none() {
            return Err(format!(
                "{agent} has no account folders: only claude, codex and gemini do"
            ));
        }
        if !valid_name(name) {
            return Err(format!(
                "'{name}' is not an account name: 1 to 32 of a-z, 0-9 and -, not starting with -"
            ));
        }
        let root = root(env)?;
        let meta =
            std::fs::metadata(&root).map_err(|error| format!("{}: {error}", root.display()))?;
        private(&root, &meta)?;
        let parent = root.join(agent);
        let dir = parent.join(format!("{agent}-{name}"));
        own_dir(&parent)?;
        own_dir(&dir)?;
        Ok(Self {
            agent: agent.to_owned(),
            name: name.to_owned(),
            dir,
        })
    }

    pub fn key(&self) -> String {
        format!("{}@{}", self.agent, self.name)
    }

    pub fn variable(&self) -> &'static str {
        variable(&self.agent).unwrap_or_default()
    }
}

fn root(env: Lookup) -> Result<PathBuf, String> {
    let root = env("AGENT_ACCOUNTS_DIR").ok_or("AGENT_ACCOUNTS_DIR is not set")?;
    // The folder reaches a tick through `env VAR=folder`, one line of `harness accounts env`.
    if !root.starts_with('/') || root.chars().any(char::is_control) {
        return Err(format!(
            "AGENT_ACCOUNTS_DIR: '{root}' is not an absolute path"
        ));
    }
    Ok(PathBuf::from(root))
}

fn own_dir(path: &Path) -> Result<(), String> {
    let meta =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", path.display()));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    private(path, &meta)
}

fn private(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(format!("{} is another user's", path.display()));
    }
    if meta.mode() & 0o002 != 0 {
        return Err(format!("{} is writable by every user", path.display()));
    }
    Ok(())
}

/// Every `<agent>-<name>` entry under the root's agent folders whose name is an account name, as
/// (agent, name), sorted.
pub fn found(env: Lookup) -> Result<Vec<(String, String)>, String> {
    let root = root(env)?;
    let mut found = Vec::new();
    for (agent, _) in AGENTS {
        let Ok(entries) = std::fs::read_dir(root.join(agent)) else {
            continue;
        };
        let prefix = format!("{agent}-");
        for entry in entries.take(DIR_LIMIT).flatten() {
            if let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .filter(|name| valid_name(name))
            {
                found.push((agent.to_owned(), name.to_owned()));
            }
        }
    }
    found.sort();
    Ok(found)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignState {
    SignedIn,
    SignedOut,
    /// The CLI gave no answer this reads, or has no way to ask: it blocks nothing.
    Unknown,
}

/// What an account's CLI said of its sign-in, and when it was asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignIn {
    pub state: SignState,
    /// How it is signed in, a word such as `claude.ai`, `api_key` or `chatgpt`.
    pub method: Option<String>,
    /// When the sign-in ends, an ISO time, where the CLI says.
    pub expires_at: Option<String>,
    pub checked_at: u64,
}

fn method_word(text: &str) -> Option<String> {
    (!text.is_empty()
        && text.len() <= 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)))
    .then(|| text.to_owned())
}

/// `claude auth status --json`: its `loggedIn`, `authMethod` and `loginExpiresAt`. The email and
/// organization it prints beside them are never read.
fn parse_claude(stdout: &str) -> (SignState, Option<String>, Option<String>) {
    let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(stdout.trim()) else {
        return (SignState::Unknown, None, None);
    };
    let state = match doc.get("loggedIn") {
        Some(Value::Bool(true)) => SignState::SignedIn,
        Some(Value::Bool(false)) => SignState::SignedOut,
        _ => SignState::Unknown,
    };
    let method = doc
        .get("authMethod")
        .and_then(Value::as_str)
        .filter(|method| *method != "none")
        .and_then(method_word);
    let expires_at = doc
        .get("loginExpiresAt")
        .and_then(Value::as_str)
        .filter(|at| {
            at.len() <= 40
                && at
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || b"-:.TZ+".contains(&byte))
                && epoch_of(at).is_some()
        })
        .map(str::to_owned);
    (state, method, expires_at)
}

/// `codex login status`, which answers on stderr: `Logged in using ChatGPT`, `Logged in using an
/// API key - <the key, masked>` or `Not logged in`. Only the method is kept, never the rest of the
/// line.
fn parse_codex(output: &str) -> (SignState, Option<String>) {
    for line in output.lines().map(str::trim) {
        if line == "Not logged in" {
            return (SignState::SignedOut, None);
        }
        if let Some(how) = line.strip_prefix("Logged in using ") {
            let method = if how == "ChatGPT" {
                Some("chatgpt".to_owned())
            } else if how.starts_with("an API key") {
                Some("api_key".to_owned())
            } else {
                None
            };
            return (SignState::SignedIn, method);
        }
    }
    (SignState::Unknown, None)
}

/// Asks the account's CLI, with its variable set to the folder, whether it is signed in. Gemini's CLI
/// has no command that answers without starting a session, so its accounts read as unknown.
fn probe(env: Lookup, account: &Account, now: u64) -> SignIn {
    let bin = env(&format!("AGENT_BIN_{}", account.agent)).unwrap_or_else(|| account.agent.clone());
    let words: &[&str] = match account.agent.as_str() {
        "claude" => &["auth", "status", "--json"],
        "codex" => &["login", "status"],
        _ => &[],
    };
    let mut sign_in = SignIn {
        state: SignState::Unknown,
        method: None,
        expires_at: None,
        checked_at: now,
    };
    if words.is_empty() {
        return sign_in;
    }
    let argv: Vec<String> = std::iter::once(bin)
        .chain(words.iter().map(|word| (*word).to_owned()))
        .collect();
    let environment = Environment::Inherit(vec![(
        account.variable().to_owned(),
        account.dir.display().to_string(),
    )]);
    let Ok(output) = command::run(&argv, environment, STATUS_TIMEOUT, STATUS_OUTPUT) else {
        return sign_in;
    };
    if account.agent == "claude" {
        (sign_in.state, sign_in.method, sign_in.expires_at) = parse_claude(&output.stdout);
    } else {
        (sign_in.state, sign_in.method) =
            parse_codex(&format!("{}\n{}", output.stdout, output.stderr));
    }
    sign_in
}

fn status_dir(state: &Path) -> PathBuf {
    state.join("accounts")
}

/// What `<state>/accounts/<key>.json` holds: the last check, and the state the launcher last logged.
#[derive(Serialize, Deserialize)]
struct Kept {
    #[serde(flatten)]
    sign_in: SignIn,
    #[serde(default)]
    logged: Option<SignState>,
}

fn kept(state: &Path, key: &str) -> Option<Kept> {
    files::read_text_limited(&status_dir(state).join(format!("{key}.json")), STATUS_FILE)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// The last check of the account `key`, when one was made.
pub fn cached(state: &Path, key: &str) -> Option<SignIn> {
    kept(state, key).map(|kept| kept.sign_in)
}

/// Checks again each account whose last check is older than [`STATUS_TTL_S`], unless another
/// process is checking now. An answer this cannot read keeps the state an earlier check found: a
/// CLI that hangs once must not let a signed-out account run. With `log`, as the launcher's refresh
/// asks, returns a line for each account signed out since the launcher last logged it, and for each
/// signed in again, so a sign-out is logged once whichever check found it.
pub fn refresh(
    env: Lookup,
    state: &Path,
    accounts: &[Account],
    now: u64,
    log: bool,
) -> Vec<String> {
    let dir = status_dir(state);
    if accounts.is_empty() || files::prepare_private_dir(&dir).is_err() {
        return Vec::new();
    }
    let Ok(lock) = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(dir.join("lock"))
    else {
        return Vec::new();
    };
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for account in accounts {
        let key = account.key();
        let before = kept(state, &key);
        let stale = before
            .as_ref()
            .is_none_or(|last| now.saturating_sub(last.sign_in.checked_at) >= STATUS_TTL_S);
        let mut after = match before {
            Some(last) if !stale => last,
            Some(last) => {
                let mut found = probe(env, account, now);
                if found.state == SignState::Unknown && last.sign_in.state != SignState::Unknown {
                    found = SignIn {
                        checked_at: now,
                        ..last.sign_in
                    };
                }
                Kept {
                    sign_in: found,
                    logged: last.logged,
                }
            }
            None => Kept {
                sign_in: probe(env, account, now),
                logged: None,
            },
        };
        let changed = log
            && after.sign_in.state != SignState::Unknown
            && after.logged != Some(after.sign_in.state);
        if changed {
            match (after.logged, after.sign_in.state) {
                (_, SignState::SignedOut) => lines.push(format!(
                    "{key} is signed out; none of its ticks start until it signs in again"
                )),
                (Some(SignState::SignedOut), _) => lines.push(format!("{key} is signed in again")),
                _ => {}
            }
            after.logged = Some(after.sign_in.state);
        }
        if (stale || changed)
            && let Ok(text) = serde_json::to_string(&after)
        {
            let _ = files::write_replacing(&dir.join(format!("{key}.json")), &text);
        }
    }
    lines
}

fn env_command(env: Lookup, keys: &[String]) -> ExitCode {
    let mut failed = false;
    for key in keys {
        let resolved = match usage::split_key(key) {
            Ok((agent, Some(name))) => Account::resolve(env, &agent, &name),
            Ok((_, None)) => Err("names no account".to_owned()),
            Err(message) => Err(message),
        };
        match resolved {
            Ok(account) => println!("{}={}", account.variable(), account.dir.display()),
            Err(message) => {
                eprintln!("{key}: {message}");
                failed = true;
            }
        }
    }
    ExitCode::from(u8::from(failed))
}

/// Every account folder found, each with its reading, its cap and its last sign-in check.
pub fn usages(env: Lookup) -> Result<Vec<AgentUsage>, String> {
    let settings = usage::Settings::from_env(env)?;
    found(env)?
        .iter()
        .map(|(agent, name)| usage::agent_usage(env, &settings, agent, Some(name), false))
        .collect()
}

/// The days left before `expires_at`, rounded down; negative once it has passed.
pub fn days_left(expires_at: &str, now: u64) -> Option<i64> {
    let at = i64::try_from(epoch_of(expires_at)?).ok()?;
    let now = i64::try_from(now).ok()?;
    Some((at - now).div_euclid(86_400))
}

/// The sign-in as one phrase, for a line of text.
pub fn describe(sign_in: Option<&SignIn>, now: u64) -> String {
    let Some(sign_in) = sign_in else {
        return "sign-in not checked yet".to_owned();
    };
    let how = sign_in
        .method
        .as_ref()
        .map(|method| format!(" ({method})"))
        .unwrap_or_default();
    match sign_in.state {
        SignState::SignedOut => "signed out".to_owned(),
        SignState::Unknown => "sign-in unknown".to_owned(),
        SignState::SignedIn => match &sign_in.expires_at {
            Some(at) => match days_left(at, now) {
                Some(days) if days < 0 => format!("signed in{how}, ended {at}"),
                Some(days) if days < 3 => {
                    format!("signed in{how}, ends {at}, in under 3 days: sign in again soon")
                }
                _ => format!("signed in{how}, ends {at}"),
            },
            None => format!("signed in{how}"),
        },
    }
}

fn report(env: Lookup, json: bool) -> Result<ExitCode, String> {
    if env("AGENT_ACCOUNTS_DIR").is_none() {
        if json {
            println!(r#"{{"accounts": []}}"#);
        } else {
            println!("accounts off: AGENT_ACCOUNTS_DIR is not set");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let now = now_s();
    if let Some(state) = usage::state_dir(env) {
        let accounts: Vec<Account> = found(env)?
            .iter()
            .filter_map(|(agent, name)| Account::resolve(env, agent, name).ok())
            .collect();
        refresh(env, &state, &accounts, now, false);
    }
    let usages = usages(env)?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({ "accounts": usages }))
                .map_err(|error| error.to_string())?
        );
        return Ok(ExitCode::SUCCESS);
    }
    if usages.is_empty() {
        println!(
            "no account folder under {}",
            env("AGENT_ACCOUNTS_DIR").unwrap_or_default()
        );
    }
    for usage in &usages {
        let reading = match &usage.reading {
            usage::Freshness::Unreadable(message) => format!(", no reading ({message})"),
            usage::Freshness::Stale => ", the reading is stale".to_owned(),
            _ => String::new(),
        };
        let mut windows = String::new();
        for window in &usage.windows {
            let _ = write!(windows, " {}={}%", window.name, window.used_pct);
        }
        println!(
            "{}: {}; cap {}{reading}{windows}",
            usage.key(),
            describe(usage.sign_in.as_ref(), now),
            usage.cap
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("accounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_are_lowercase_words_and_map_to_setting_names() {
        for good in ["work", "a", "client-2", "0", &"x".repeat(32)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "-a",
            "Work",
            "a_b",
            "a.b",
            "a/b",
            "a b",
            &"x".repeat(33),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(key_name("client-2"), "client_2");
    }

    #[test]
    fn each_agent_resolves_to_its_own_folder_and_variable() {
        let root = scratch("resolve");
        for agent in ["claude", "codex", "gemini"] {
            std::fs::create_dir_all(root.join(agent).join(format!("{agent}-work"))).unwrap();
        }
        let env = env_of(&[("AGENT_ACCOUNTS_DIR", root.to_str().unwrap())]);
        let claude = Account::resolve(&env, "claude", "work").unwrap();
        assert_eq!(claude.dir, root.join("claude/claude-work"));
        assert_eq!(claude.variable(), "CLAUDE_CONFIG_DIR");
        assert_eq!(claude.key(), "claude@work");
        assert_eq!(
            Account::resolve(&env, "codex", "work").unwrap().variable(),
            "CODEX_HOME"
        );
        assert_eq!(
            Account::resolve(&env, "gemini", "work").unwrap().variable(),
            "GEMINI_CLI_HOME"
        );
        assert!(Account::resolve(&env, "aider", "work").is_err());
        assert!(
            Account::resolve(&env, "claude", "home")
                .unwrap_err()
                .contains("claude-home")
        );
        assert_eq!(
            found(&env).unwrap(),
            [
                ("claude".to_owned(), "work".to_owned()),
                ("codex".to_owned(), "work".to_owned()),
                ("gemini".to_owned(), "work".to_owned())
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symlinked_or_world_writable_folder_is_refused() {
        let root = scratch("refuse");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(root.join("claude/claude-open")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("claude/claude-link")).unwrap();
        std::fs::set_permissions(
            root.join("claude/claude-open"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("codex/codex-work")).unwrap();
        std::fs::set_permissions(root.join("codex"), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        let env = env_of(&[("AGENT_ACCOUNTS_DIR", root.to_str().unwrap())]);
        let error = Account::resolve(&env, "claude", "link").unwrap_err();
        assert!(error.ends_with("is a symlink"), "{error}");
        let error = Account::resolve(&env, "claude", "open").unwrap_err();
        assert!(error.ends_with("writable by every user"), "{error}");
        let error = Account::resolve(&env, "codex", "work").unwrap_err();
        assert!(error.contains("/codex is writable"), "{error}");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = Account::resolve(&env, "claude", "open").unwrap_err();
        assert!(
            error.starts_with(&format!("{} is writable", root.display())),
            "{error}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_root_must_be_set_and_absolute() {
        assert!(
            Account::resolve(&env_of(&[]), "claude", "work")
                .unwrap_err()
                .contains("not set")
        );
        let relative = env_of(&[("AGENT_ACCOUNTS_DIR", "accounts")]);
        assert!(
            Account::resolve(&relative, "claude", "work")
                .unwrap_err()
                .contains("not an absolute path")
        );
    }

    #[test]
    fn claude_keeps_only_its_state_method_and_end() {
        let signed_in = r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
            "loginExpiresAt": "2026-12-15T12:26:36.691Z", "email": "someone@example.com",
            "orgId": "00000000-0000-0000-0000-000000000000", "orgName": "Example Org",
            "subscriptionType": "max"}"#;
        assert_eq!(
            parse_claude(signed_in),
            (
                SignState::SignedIn,
                Some("claude.ai".to_owned()),
                Some("2026-12-15T12:26:36.691Z".to_owned())
            )
        );
        let signed_out = r#"{"loggedIn": false, "authMethod": "none", "loginExpiresAt": null}"#;
        assert_eq!(parse_claude(signed_out), (SignState::SignedOut, None, None));
        let api_key =
            r#"{"loggedIn": true, "authMethod": "api_key", "apiKeySource": "ANTHROPIC_API_KEY"}"#;
        assert_eq!(
            parse_claude(api_key),
            (SignState::SignedIn, Some("api_key".to_owned()), None)
        );
        let odd =
            r#"{"loggedIn": true, "authMethod": "a b@c", "loginExpiresAt": "someone@example.com"}"#;
        assert_eq!(parse_claude(odd), (SignState::SignedIn, None, None));
        assert_eq!(parse_claude("not json"), (SignState::Unknown, None, None));
        assert_eq!(parse_claude(r#"{"loggedIn": "yes"}"#).0, SignState::Unknown);
    }

    #[test]
    fn codex_keeps_only_its_state_and_method() {
        assert_eq!(
            parse_codex("\nLogged in using ChatGPT\n"),
            (SignState::SignedIn, Some("chatgpt".to_owned()))
        );
        assert_eq!(
            parse_codex("Logged in using an API key - sk-proj-***67890"),
            (SignState::SignedIn, Some("api_key".to_owned()))
        );
        assert_eq!(parse_codex("Not logged in"), (SignState::SignedOut, None));
        assert_eq!(
            parse_codex("Error checking login status: expected ident"),
            (SignState::Unknown, None)
        );
    }

    /// Writes an executable from a child process. A file this process held open for writing could
    /// still be open in a child that another test thread forked and has not yet exec'd, and running
    /// it then fails with ETXTBSY.
    fn write_executable(path: &Path, text: &str) {
        use std::io::Write as _;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        assert!(
            child.wait().unwrap().success(),
            "writing {}",
            path.display()
        );
    }

    fn refresh_logged(env: Lookup, state: &Path, account: &Account, now: u64) -> Vec<String> {
        refresh(env, state, std::slice::from_ref(account), now, true)
    }

    #[test]
    fn a_check_is_cached_and_a_sign_out_is_reported_once() {
        let root = scratch("probe");
        let state = root.join("state");
        std::fs::create_dir_all(root.join("accounts/codex/codex-work")).unwrap();
        let answer = root.join("answer");
        let fake = root.join("codex");
        write_executable(
            &fake,
            &format!(
                "#!/bin/sh\necho \"$CODEX_HOME\" >> {0}.seen\ncat {0} >&2\n",
                answer.display()
            ),
        );
        let env = env_of(&[
            (
                "AGENT_ACCOUNTS_DIR",
                root.join("accounts").to_str().unwrap(),
            ),
            ("AGENT_BIN_codex", fake.to_str().unwrap()),
        ]);
        let account = Account::resolve(&env, "codex", "work").unwrap();
        std::fs::write(&answer, "Not logged in\n").unwrap();
        let first = refresh_logged(&env, &state, &account, 1_000);
        assert_eq!(
            first,
            ["codex@work is signed out; none of its ticks start until it signs in again"]
        );
        assert_eq!(
            cached(&state, "codex@work").unwrap().state,
            SignState::SignedOut
        );
        let within = refresh_logged(&env, &state, &account, 1_100);
        let again = refresh_logged(&env, &state, &account, 1_400);
        assert!(
            within.is_empty() && again.is_empty(),
            "{within:?} {again:?}"
        );
        let seen = std::fs::read_to_string(root.join("answer.seen")).unwrap();
        assert_eq!(
            seen.lines().count(),
            2,
            "the check within the hold ran nothing"
        );
        assert!(
            seen.lines()
                .all(|line| line == account.dir.to_str().unwrap())
        );
        std::fs::write(&answer, "Error checking login status: timed out\n").unwrap();
        assert!(refresh_logged(&env, &state, &account, 1_800).is_empty());
        assert_eq!(
            cached(&state, "codex@work").unwrap(),
            SignIn {
                state: SignState::SignedOut,
                method: None,
                expires_at: None,
                checked_at: 1_800
            },
            "an answer this cannot read keeps the state found before"
        );
        std::fs::write(&answer, "Logged in using ChatGPT\n").unwrap();
        assert!(
            refresh(&env, &state, std::slice::from_ref(&account), 2_200, false).is_empty(),
            "a check that does not log leaves the line to the launcher"
        );
        let back = refresh_logged(&env, &state, &account, 2_300);
        assert_eq!(back, ["codex@work is signed in again"]);
        let file = std::fs::read_to_string(state.join("accounts/codex@work.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&file).unwrap(),
            serde_json::json!({"state": "signed_in", "method": "chatgpt", "expires_at": null, "checked_at": 2200, "logged": "signed_in"})
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_missing_cli_or_gemini_reads_unknown() {
        let root = scratch("unknown");
        std::fs::create_dir_all(root.join("gemini/gemini-a")).unwrap();
        std::fs::create_dir_all(root.join("claude/claude-a")).unwrap();
        let env = env_of(&[
            ("AGENT_ACCOUNTS_DIR", root.to_str().unwrap()),
            ("AGENT_BIN_claude", "/nonexistent/claude"),
        ]);
        for agent in ["gemini", "claude"] {
            let account = Account::resolve(&env, agent, "a").unwrap();
            assert_eq!(probe(&env, &account, 5).state, SignState::Unknown);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_sign_in_reads_as_a_phrase_with_a_warning_near_its_end() {
        let now = epoch_of("2026-12-01T00:00:00Z").unwrap();
        let at = |expires: &str| SignIn {
            state: SignState::SignedIn,
            method: Some("claude.ai".to_owned()),
            expires_at: Some(expires.to_owned()),
            checked_at: now,
        };
        assert_eq!(
            describe(Some(&at("2026-12-15T00:00:00Z")), now),
            "signed in (claude.ai), ends 2026-12-15T00:00:00Z"
        );
        assert!(describe(Some(&at("2026-12-03T00:00:00Z")), now).contains("under 3 days"));
        assert!(describe(Some(&at("2026-11-30T00:00:00Z")), now).contains("ended"));
        assert_eq!(describe(None, now), "sign-in not checked yet");
        assert_eq!(days_left("2026-12-04T00:00:00Z", now), Some(3));
        assert_eq!(days_left("2026-11-30T12:00:00Z", now), Some(-1));
    }
}
