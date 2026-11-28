//! Masks secrets in text and reduces command lines to a short, masked summary before they reach
//! the page.
//!
//! Secret-looking values: `KEY=value` and `--flag value` / `--flag=value` where the name holds
//! TOKEN, KEY, SECRET, PASS, AUTH, CRED, COOKIE or SESSION (any case); the value of a header whose
//! name holds one of them (`Authorization:`, `X-Api-Key:`, `Cookie:`) and `Bearer` credentials;
//! URL userinfo with a password, and any userinfo of an http(s) URL; the password of
//! `curl -u user:password`, of `sshpass -p` and `docker login -p`, and of a glued `mysql -pVALUE`,
//! but no other command's `-p`, which is a port or a parent flag to most tools; GitHub, GitLab,
//! Slack, AWS and `sk-` token shapes; hex runs of 32 characters or more; and base64-like runs of 32
//! or more mixing upper case, lower case and digits.
//!
//! [`redact`] masks them in free text (a log line) shown as it is, and replaces control, bidi and
//! invisible characters. [`summarize`] works on what the shell would see: it tokenizes first, the
//! way the shell dequotes, and masks each dequoted word. A secret-named flag masks every word after
//! it up to the next flag or operator; a secret-named header word masks the words after it up to
//! the next flag or operator but `;`, which separates a cookie's pairs, an auth scheme right after
//! it kept. A word that is itself a script (a `-c` argument, any word holding spaces or quotes) is
//! tokenized and masked again, to a fixed depth past which it is masked whole. When the quoting
//! does not parse, everything from the failure on is masked. Each command's masked words then go
//! through [`redact`] once more, joined, so a pattern that spans words still applies. Only then
//! does it shorten the result to the working command's executable and a few arguments. No raw
//! command line leaves it.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use super::shlex;
use super::text::{basename, clean, is_word_char};

pub const MASK: &str = "[redacted]";
pub const MAX_SCRIPT_DEPTH: usize = 4;

const SECRET_WORDS: &str = "(?:TOKEN|KEY|SECRET|PASS|AUTH|CRED|COOKIE|SESSION)";
const ESCAPED_OR_QUOTED_VALUE: &str = r#"\\"[^"]*?\\"|\\'[^']*?\\'|"[^"]*"|'[^']*'"#;
const AUTH_SCHEMES: [&str; 4] = ["bearer", "token", "basic", "digest"];
const SCRIPT_CHARACTERS: &str = " \t\n'\"\\;|&";
const PREAMBLE_COMMANDS: [&str; 19] = [
    "cd", "export", "source", ".", "set", "unset", "true", "false", ":", "local", "umask", "shopt",
    "break", "continue", "exit", "return", "wait", "shift", "trap",
];
const LOW_SIGNAL_COMMANDS: [&str; 13] = [
    "ls", "echo", "printf", "date", "pwd", "mkdir", "touch", "test", "[", "cp", "rm", "cat", "wc",
];
const LEADING_KEYWORDS: [&str; 14] = [
    "do", "then", "else", "elif", "if", "while", "until", "!", "{", "time", "exec", "eval",
    "command", "builtin",
];
const HEADER_KEYWORDS: [&str; 7] = ["for", "select", "case", "done", "fi", "esac", "}"];
const SUBSTITUTION_PLACEHOLDER: &str = "__SUBST__";
const MYSQL_COMMANDS: [&str; 4] = ["mysql", "mysqldump", "mysqladmin", "mariadb"];
const PASSWORD_COMMANDS: [&str; 8] = [
    "mysql",
    "mysqldump",
    "mysqladmin",
    "mariadb",
    "curl",
    "sshpass",
    "docker",
    "podman",
];
const PUNCTUATION_CHARACTERS: &str = "();<>|&";

fn regex(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("a constant pattern compiles")
}

static SECRET_NAME: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(&format!("(?i){SECRET_WORDS}")));
static SECRET_FLAG_WORD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)^--?[A-Za-z0-9-]*{SECRET_WORDS}[A-Za-z0-9-]*\n?$"
    ))
});
static NAME: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"^(?:[A-Za-z_][A-Za-z0-9_]*|--?[A-Za-z0-9][A-Za-z0-9-]*)\n?$"));
static SECRET_ASSIGNMENT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)([A-Za-z0-9_]*{SECRET_WORDS}[A-Za-z0-9_]*)=({ESCAPED_OR_QUOTED_VALUE}|[^\s;&|]+)"
    ))
});
// The look-around of the patterns below is checked in code (see `replace_checked`), so every
// pattern stays one the linear-time engine runs: no text can make matching slow.
static SECRET_FLAG: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)(--?[A-Za-z0-9-]*{SECRET_WORDS}[A-Za-z0-9-]*)(=|\s+)({ESCAPED_OR_QUOTED_VALUE}|[^\s;&|]+)"
    ))
});
static ESCAPED_OR_QUOTED_START: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(&format!("^(?:{ESCAPED_OR_QUOTED_VALUE})")));
static SECRET_FLAG_START: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)^--?[A-Za-z0-9-]*{SECRET_WORDS}[A-Za-z0-9-]*(?:[\s=]|$)"
    ))
});
static SECRET_HEADER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?i)([A-Za-z0-9_-]*{SECRET_WORDS}[A-Za-z0-9_-]*\s*:\s*)((?:bearer|token|basic|digest)\s+)?([^\s'"]+)"#
    ))
});
static CURL_USER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"(curl()[^;&|]*?\s(?:-u\s*|--user(?:=|\s+))[^\s:;&|]+:)[^\s;&|]+"));
static PASSWORD_FLAG: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(
        r"((?:sshpass|(?:docker|podman)\s+login)()[^;&|]*?\s-p\s*|(?:mysqldump|mysqladmin|mysql|mariadb)()[^;&|]*?\s-p)[^\s;&|]+",
    )
});
static HEADER_WORD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"^([A-Za-z0-9_-]+):(\S*)\n?$"));
static FLAG_WORD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"^--?[A-Za-z][A-Za-z0-9_-]*(?:=.*)?\n?$"));
static BEARER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]+"));
static URL_PASSWORD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r#"(?i)([a-z][a-z0-9+.-]*://)[^/\s@:'"]+:[^/\s@'"]*@"#));
static HTTP_USERINFO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r#"(?i)(https?://)[^/\s@:'"]+@"#));
static TOKEN_SHAPE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(concat!(
        r"(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}",
        r"|xox[abposr]-[A-Za-z0-9-]{10,}|sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16})"
    ))
});
static ALNUM_RUN: LazyLock<regex::Regex> = LazyLock::new(|| regex("[0-9A-Za-z]+"));
static BASE64_RUN: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"[A-Za-z0-9+/_-]{32,}={0,2}"));
static SUBSTITUTION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"\$\(\([^()]*\)\)|\$\([^()]*\)|`[^`]*`"));
static ASSIGNMENT: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"^[A-Za-z_][A-Za-z0-9_]*="));

/// Replaces the matches of `pattern` that `accept` takes, as a backtracking engine with
/// look-around would: `accept` gets a match and returns its end and replacement, or refuses it,
/// and the search then goes on one character after where the refused match started.
fn replace_checked(
    pattern: &regex::Regex,
    text: &str,
    accept: impl Fn(&regex::Captures<'_>) -> Option<(usize, String)>,
) -> String {
    let mut output = String::with_capacity(text.len());
    let mut last = 0;
    let mut position = 0;
    while let Some(found) = pattern.captures_at(text, position) {
        let Some(whole) = found.get(0) else { break };
        if let Some((end, replacement)) = accept(&found) {
            output.push_str(&text[last..whole.start()]);
            output.push_str(&replacement);
            last = end;
            position = end;
        } else {
            let Some(next) = text[whole.start()..].chars().next() else {
                break;
            };
            position = whole.start() + next.len_utf8();
        }
        if position > text.len() {
            break;
        }
    }
    output.push_str(&text[last..]);
    output
}

/// True when the character before `index` is not one of those `excluded` names: the
/// look-behind `(?<![...])`.
fn preceded_by_none(text: &str, index: usize, excluded: impl Fn(char) -> bool) -> bool {
    !text[..index].chars().next_back().is_some_and(excluded)
}

fn word_or_dash(character: char) -> bool {
    is_word_char(character) || character == '-'
}

fn word_dot_or_dash(character: char) -> bool {
    word_or_dash(character) || character == '.'
}

fn group<'t>(captures: &regex::Captures<'t>, index: usize) -> &'t str {
    captures.get(index).map_or("", |found| found.as_str())
}

/// Masks every secret-looking value in free text shown as it is, quotes and escapes included,
/// and replaces control, bidi and invisible characters.
pub fn redact(text: &str) -> String {
    let text = clean(text);
    let masked = replace_checked(&SECRET_ASSIGNMENT, &text, |found| {
        let whole = found.get(0)?;
        preceded_by_none(&text, whole.start(), is_word_char)
            .then(|| (whole.end(), format!("{}={MASK}", group(found, 1))))
    });
    let masked = replace_checked(&SECRET_FLAG, &masked, |found| {
        let whole = found.get(0)?;
        let value = found.get(3)?;
        // The value is a word that is no secret flag itself, unless it is a quoted value.
        let quoted = ESCAPED_OR_QUOTED_START.is_match(value.as_str());
        let accepted = preceded_by_none(&masked, whole.start(), word_or_dash)
            && (quoted || !SECRET_FLAG_START.is_match(&masked[value.start()..]));
        accepted.then(|| {
            (
                whole.end(),
                format!("{}{}{MASK}", group(found, 1), group(found, 2)),
            )
        })
    });
    // The empty groups mark where the command's name ends, which must be a word boundary.
    let command_flag = |text: &str, found: &regex::Captures<'_>| {
        let whole = found.get(0)?;
        let name_end = (2..found.len()).find_map(|index| found.get(index))?.end();
        let bounded = !text[name_end..].chars().next().is_some_and(is_word_char);
        (bounded && preceded_by_none(text, whole.start(), word_dot_or_dash))
            .then(|| (whole.end(), format!("{}{MASK}", group(found, 1))))
    };
    let masked = replace_checked(&CURL_USER, &masked, |found| command_flag(&masked, found));
    let masked = replace_checked(&PASSWORD_FLAG, &masked, |found| {
        command_flag(&masked, found)
    });
    let masked = replace_checked(&SECRET_HEADER, &masked, |found| {
        let whole = found.get(0)?;
        let name = found.get(1)?;
        if !preceded_by_none(&masked, whole.start(), word_or_dash) {
            return None;
        }
        let replacement = format!("{}{MASK}", name.as_str());
        if !group(found, 3).starts_with("//") {
            return Some((whole.end(), replacement));
        }
        // A value that is a URL path is not one: without the auth scheme, the scheme word is
        // the value instead.
        found.get(2)?;
        let rest = &masked[name.end()..];
        let length = rest
            .find(|character: char| {
                character.is_whitespace() || character == '\'' || character == '"'
            })
            .unwrap_or(rest.len());
        Some((name.end() + length, replacement))
    });
    let at_word_start = |text: &str, found: &regex::Captures<'_>, suffix: &str| {
        let whole = found.get(0)?;
        preceded_by_none(text, whole.start(), is_word_char)
            .then(|| (whole.end(), format!("{}{MASK}{suffix}", group(found, 1))))
    };
    let masked = replace_checked(&BEARER, &masked, |found| at_word_start(&masked, found, ""));
    let masked = replace_checked(&URL_PASSWORD, &masked, |found| {
        at_word_start(&masked, found, "@")
    });
    let masked = replace_checked(&HTTP_USERINFO, &masked, |found| {
        at_word_start(&masked, found, "@")
    });
    let masked = TOKEN_SHAPE.replace_all(&masked, MASK);
    let masked = ALNUM_RUN.replace_all(&masked, |found: &regex::Captures<'_>| {
        let run = group(found, 0);
        if run.len() >= 32 && run.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            MASK.to_owned()
        } else {
            run.to_owned()
        }
    });
    BASE64_RUN
        .replace_all(&masked, |found: &regex::Captures<'_>| {
            let run = group(found, 0);
            if looks_random(run) {
                MASK.to_owned()
            } else {
                run.to_owned()
            }
        })
        .into_owned()
}

/// Reduces a command line to its working command's executable and a few masked, shortened
/// arguments: `exe arg ...`, `+N more` when further commands follow; empty for an empty command.
pub fn summarize(command: &str, max_arguments: usize, max_argument_chars: usize) -> String {
    summarize_words(
        &redact_command(command, 0),
        max_arguments,
        max_argument_chars,
        true,
    )
}

/// Summarises a process's argv the same way; a script run by an interpreter is named by the
/// script.
pub fn summarize_argv<S: AsRef<str>>(argv: &[S], max_arguments: usize) -> String {
    let mut words: Vec<String> = argv.iter().map(|word| word.as_ref().to_owned()).collect();
    if words.len() > 1
        && ["bash", "sh", "python3", "python"].contains(&basename(&words[0]))
        && !words[1].starts_with('-')
    {
        words.remove(0);
    }
    summarize_words(&redact_words(&words, 0), max_arguments, 32, true)
}

/// Tokenizes a command line as the shell would and masks each dequoted word; MASK stands for
/// everything from a quoting failure on.
pub fn redact_command(command: &str, depth: usize) -> Vec<String> {
    let mut collapsed = command.to_owned();
    loop {
        let replaced = SUBSTITUTION
            .replace_all(&collapsed, SUBSTITUTION_PLACEHOLDER)
            .into_owned();
        if replaced == collapsed {
            break;
        }
        collapsed = replaced;
    }
    let split = shlex::split_command(&collapsed);
    let mut redacted = redact_words(&split.words, depth);
    if split.error.is_some() {
        redacted.push(MASK.to_owned());
    }
    let joined = redacted.join("\n");
    let backstopped = redact(&joined);
    if backstopped != joined {
        redacted = backstopped
            .split('\n')
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect();
    }
    redacted
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Masking {
    None,
    Flag,
    Header,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Owed {
    Nothing,
    Password,
    Userinfo,
}

/// Masks a list of dequoted words.
///
/// A secret-named flag (`--token`) masks every word after it up to the next flag or operator.
/// The first word after it is masked whatever its shape, since a secret value may start with a
/// dash, unless that word is itself a secret-named flag: `--token --password secret` masks
/// `secret`, and `--token -abc` masks `-abc`. A header word whose name holds a secret word
/// (`Authorization:`, `X-Api-Key:`, `Cookie:`), `Bearer`, or `Authorization:Bearer` masks the
/// words after it up to the next flag or operator but `;`, keeping an auth scheme that comes right
/// after a bare header name; a header word carrying its value (`X-Api-Key:abc`) loses that value
/// too. After a command that takes a short password flag, that flag's value is masked. Every
/// other word is masked on its own.
fn redact_words(words: &[String], depth: usize) -> Vec<String> {
    let mut redacted = Vec::with_capacity(words.len());
    let mut masking = Masking::None;
    let mut scheme_may_follow = false;
    let mut value_owed = false;
    let mut password_commands: BTreeSet<String> = BTreeSet::new();
    let mut password_owed = Owed::Nothing;
    for word in words {
        if is_punctuation(word) {
            redacted.push(word.clone());
            password_commands.clear();
            password_owed = Owed::Nothing;
            if !(masking == Masking::Header && word == ";") {
                masking = Masking::None;
                value_owed = false;
            }
            scheme_may_follow = false;
            continue;
        }
        if password_owed != Owed::Nothing {
            redacted.push(if password_owed == Owed::Password {
                MASK.to_owned()
            } else {
                mask_userinfo(word)
            });
            password_owed = Owed::Nothing;
            continue;
        }
        if masking != Masking::None {
            let taken_as_flag = if value_owed {
                SECRET_FLAG_WORD.is_match(word)
            } else {
                FLAG_WORD.is_match(word)
            };
            if !taken_as_flag {
                if scheme_may_follow && AUTH_SCHEMES.contains(&word.to_lowercase().as_str()) {
                    redacted.push(word.clone());
                } else {
                    redacted.push(MASK.to_owned());
                    value_owed = false;
                }
                scheme_may_follow = false;
                continue;
            }
        }
        masking = Masking::None;
        scheme_may_follow = false;
        value_owed = false;
        let name = basename(word);
        if PASSWORD_COMMANDS.contains(&name) || word == "login" {
            password_commands.insert(name.to_owned());
        }
        let (password_word, owed) = password_flag(word, &password_commands);
        if password_word != *word || owed != Owed::Nothing {
            redacted.push(password_word);
            password_owed = owed;
            continue;
        }
        if SECRET_FLAG_WORD.is_match(word) {
            redacted.push(word.clone());
            masking = Masking::Flag;
            value_owed = true;
            continue;
        }
        if word.to_lowercase() == "bearer" {
            redacted.push(word.clone());
            masking = Masking::Header;
            value_owed = true;
            continue;
        }
        if let Some(header) = HEADER_WORD.captures(word) {
            let (header_name, value) = (group(&header, 1), group(&header, 2));
            if SECRET_NAME.is_match(header_name) && !value.starts_with("//") {
                masking = Masking::Header;
                if value.is_empty() {
                    redacted.push(word.clone());
                    scheme_may_follow = true;
                    value_owed = true;
                } else if AUTH_SCHEMES.contains(&value.to_lowercase().as_str()) {
                    redacted.push(word.clone());
                    value_owed = true;
                } else {
                    redacted.push(format!("{header_name}:{MASK}"));
                }
                continue;
            }
        }
        redacted.push(redact_word(word, depth));
    }
    redacted
}

/// Masks a short password flag, read only once a command that takes one has appeared in this
/// simple command, wrappers such as `sudo` or `timeout` allowed before it: to `ssh`, `mkdir` and
/// most others `-p` is a port or a parent flag. `mysql -p` alone prompts, so only its glued
/// `-pVALUE` carries a password. Returns the word, a glued value masked, and what the next word
/// owes.
fn password_flag(word: &str, commands: &BTreeSet<String>) -> (String, Owed) {
    if commands.contains("curl") {
        if word == "-u" || word == "--user" {
            return (word.to_owned(), Owed::Userinfo);
        }
        for flag in ["--user=", "-u"] {
            if let Some(value) = word.strip_prefix(flag)
                && !value.is_empty()
            {
                return (format!("{flag}{}", mask_userinfo(value)), Owed::Nothing);
            }
        }
    }
    let spaced = commands.contains("sshpass")
        || (commands.contains("login")
            && (commands.contains("docker") || commands.contains("podman")));
    if word == "-p" && spaced {
        return (word.to_owned(), Owed::Password);
    }
    if word.starts_with("-p")
        && word.len() > 2
        && (spaced
            || MYSQL_COMMANDS
                .iter()
                .any(|command| commands.contains(*command)))
    {
        return (format!("-p{MASK}"), Owed::Nothing);
    }
    (word.to_owned(), Owed::Nothing)
}

/// A `user:password` value with the part after the first colon masked; a bare user makes curl
/// prompt for the password.
fn mask_userinfo(value: &str) -> String {
    match value.split_once(':') {
        Some((user, _)) => format!("{user}:{MASK}"),
        None => value.to_owned(),
    }
}

/// Masks one dequoted word: a secret-named `name=value` loses its whole value, a word that is a
/// script is tokenized and masked in turn, and any other word goes through the free-text masks.
fn redact_word(word: &str, depth: usize) -> String {
    if let Some((name, _)) = word.split_once('=')
        && NAME.is_match(name)
        && SECRET_NAME.is_match(name)
    {
        return format!("{name}={MASK}");
    }
    if word
        .chars()
        .any(|character| SCRIPT_CHARACTERS.contains(character))
    {
        if depth >= MAX_SCRIPT_DEPTH {
            return MASK.to_owned();
        }
        return redact_command(word, depth + 1).join(" ");
    }
    redact(word)
}

/// Picks the working command among masked words and keeps its executable and first arguments.
fn summarize_words(
    words: &[String],
    max_arguments: usize,
    max_argument_chars: usize,
    allow_nested: bool,
) -> String {
    let commands = simple_commands(words);
    if commands.is_empty() {
        return String::new();
    }
    let mut chosen_index = None;
    let mut fallback_index = None;
    for (index, command_words) in commands.iter().enumerate() {
        let executable = basename(&command_words[0]);
        if PREAMBLE_COMMANDS.contains(&executable)
            || command_words[0].starts_with('$')
            || command_words[0].starts_with(SUBSTITUTION_PLACEHOLDER)
        {
            continue;
        }
        if fallback_index.is_none() {
            fallback_index = Some(index);
        }
        if !LOW_SIGNAL_COMMANDS.contains(&executable) {
            chosen_index = Some(index);
            break;
        }
    }
    let chosen_index = chosen_index.or(fallback_index).unwrap_or(0);
    let chosen = &commands[chosen_index];
    let mut arguments = Vec::new();
    let mut index = 1;
    while index < chosen.len() {
        let word = &chosen[index];
        let following = chosen.get(index + 1).map_or("", String::as_str);
        if is_redirection(word) {
            index += 2;
            continue;
        }
        if !word.is_empty()
            && word.bytes().all(|byte| byte.is_ascii_digit())
            && is_redirection(following)
        {
            index += 1;
            continue;
        }
        if allow_nested && chosen[index - 1] == "-c" && word.trim().contains(' ') {
            let nested = summarize_words(&redact_command(word, 0), 2, max_argument_chars, false);
            arguments.push(format!("'{nested}'"));
        } else {
            arguments.push(shorten(word, max_argument_chars));
        }
        index += 1;
    }
    let executable = basename(&chosen[0]);
    let mut summary = if executable.is_empty() {
        chosen[0].clone()
    } else {
        executable.to_owned()
    };
    for argument in arguments.iter().take(max_arguments) {
        summary.push(' ');
        summary.push_str(argument);
    }
    if arguments.len() > max_arguments {
        summary.push_str(" …");
    }
    let remaining_commands = commands.len() - chosen_index - 1;
    if remaining_commands > 0 {
        summary.push_str(" +");
        summary.push_str(&remaining_commands.to_string());
        summary.push_str(" more");
    }
    summary.replace(SUBSTITUTION_PLACEHOLDER, "$(…)")
}

/// Groups masked words into simple commands: split at control operators, leading assignments
/// and `do`/`then`/`if`/`while`-style keywords dropped, and loop headers left out.
fn simple_commands(words: &[String]) -> Vec<Vec<String>> {
    let mut commands: Vec<Vec<String>> = vec![Vec::new()];
    for word in words {
        if is_punctuation(word) && !is_redirection(word) {
            if commands.last().is_some_and(|command| !command.is_empty()) {
                commands.push(Vec::new());
            }
            continue;
        }
        if let Some(command) = commands.last_mut() {
            command.push(word.clone());
        }
    }
    commands
        .into_iter()
        .filter_map(|command| {
            let start = command.iter().position(|word| {
                !(LEADING_KEYWORDS.contains(&word.as_str()) || ASSIGNMENT.is_match(word))
            })?;
            let command = command[start..].to_vec();
            (!HEADER_KEYWORDS.contains(&command[0].as_str())).then_some(command)
        })
        .collect()
}

/// True for an operator token: only `();<>|&` characters.
fn is_punctuation(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|character| PUNCTUATION_CHARACTERS.contains(character))
}

/// True for a redirection operator, which belongs to its command and takes the next word as its
/// target.
fn is_redirection(token: &str) -> bool {
    is_punctuation(token) && (token.contains('<') || token.contains('>'))
}

/// The word, a long path cut to its last part, or a long word cut with an ellipsis.
fn shorten(word: &str, max_chars: usize) -> String {
    let word = word.split_whitespace().collect::<Vec<_>>().join(" ");
    if word.chars().count() <= max_chars {
        return word;
    }
    let trimmed = word.trim_end_matches('/');
    if let Some((_, tail)) = trimmed.rsplit_once('/') {
        let tail = if tail.chars().count() <= max_chars {
            tail.to_owned()
        } else {
            format!(
                "{}…",
                tail.chars()
                    .take(max_chars.saturating_sub(1))
                    .collect::<String>()
            )
        };
        return format!("…/{tail}");
    }
    format!(
        "{}…",
        word.chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
    )
}

/// Tells a base64-like secret from a path or a name: it mixes upper case, lower case and digits,
/// with upper case at least a tenth of its letters.
fn looks_random(run: &str) -> bool {
    let uppercase = run.bytes().filter(u8::is_ascii_uppercase).count();
    let lowercase = run.bytes().filter(u8::is_ascii_lowercase).count();
    let digits = run.bytes().filter(u8::is_ascii_digit).count();
    uppercase > 0 && lowercase > 0 && digits > 0 && uppercase * 10 >= uppercase + lowercase
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::fixture_json;

    fn text(value: &serde_json::Value, key: &str) -> String {
        value[key].as_str().unwrap_or_default().to_owned()
    }

    #[test]
    fn every_secret_shape_is_masked() {
        for shape in fixture_json("secret_shapes.json")
            .as_array()
            .into_iter()
            .flatten()
        {
            let masked = redact(&format!("tool {} --next value", text(shape, "text")));
            assert!(
                !masked.contains(&text(shape, "secret")),
                "{}: {masked}",
                text(shape, "shape")
            );
            assert!(masked.contains(MASK), "{}", text(shape, "shape"));
            assert!(
                masked.contains("--next value"),
                "{}: {masked}",
                text(shape, "shape")
            );
        }
    }

    #[test]
    fn every_secret_shape_is_masked_in_a_summary() {
        for shape in fixture_json("secret_shapes.json")
            .as_array()
            .into_iter()
            .flatten()
        {
            let summary = summarize(&format!("cd /w && tool {}", text(shape, "text")), 8, 200);
            assert!(summary.starts_with("tool "), "{summary}");
            assert!(
                !summary.contains(&text(shape, "secret")),
                "{}: {summary}",
                text(shape, "shape")
            );
            assert!(summary.contains(MASK), "{}", text(shape, "shape"));
        }
    }

    fn secrets(shape: &serde_json::Value) -> Vec<String> {
        shape["secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|secret| secret.as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn quoting_bypass_shapes_are_masked_in_the_summary() {
        for shape in fixture_json("bypass_shapes.json")
            .as_array()
            .into_iter()
            .flatten()
        {
            for max_arguments in [4, 12] {
                let command = text(shape, "command");
                let summary = summarize(&command, max_arguments, 200);
                let masked_words = redact_command(&command, 0).join(" ");
                assert!(masked_words.contains(MASK), "{}", text(shape, "shape"));
                for secret in secrets(shape) {
                    assert!(
                        !summary.contains(&secret),
                        "{}: {summary}",
                        text(shape, "shape")
                    );
                    assert!(
                        !masked_words.contains(&secret),
                        "{}: {masked_words}",
                        text(shape, "shape")
                    );
                }
            }
        }
    }

    #[test]
    fn quoting_bypass_shapes_are_masked_in_an_argv_summary() {
        for shape in fixture_json("bypass_shapes.json")
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(argv) = shape["argv"].as_array() else {
                continue;
            };
            let argv: Vec<String> = argv
                .iter()
                .filter_map(|word| word.as_str().map(str::to_owned))
                .collect();
            let summary = summarize_argv(&argv, 12);
            let masked_words = redact_command(&shlex::join(&argv), 0).join(" ");
            assert!(masked_words.contains(MASK), "{}", text(shape, "shape"));
            for secret in secrets(shape) {
                assert!(
                    !summary.contains(&secret),
                    "{}: {summary}",
                    text(shape, "shape")
                );
                assert!(
                    !masked_words.contains(&secret),
                    "{}: {masked_words}",
                    text(shape, "shape")
                );
            }
        }
    }

    #[test]
    fn every_word_after_a_quoting_failure_is_masked() {
        assert_eq!(
            redact_command("tool --name \"a b", 0),
            ["tool", "--name", MASK]
        );
        assert_eq!(redact_command("tool 'a", 0), ["tool", MASK]);
        assert_eq!(
            redact_command("tool --token=x --password y z --next w", 0),
            [
                "tool",
                "--token=[redacted]",
                "--password",
                "[redacted]",
                "[redacted]",
                "--next",
                "w"
            ]
        );
    }

    #[test]
    fn scripts_nested_past_the_depth_limit_are_masked_whole() {
        let mut script = "tool --name plain".to_owned();
        for _ in 0..=MAX_SCRIPT_DEPTH {
            script = shlex::join(&["bash", "-c", &script]);
        }
        assert!(!redact_command(&script, 0).join(" ").contains("plain"));
    }

    #[test]
    fn plain_values_are_kept() {
        for kept in fixture_json("kept_shapes.json")
            .as_array()
            .into_iter()
            .flatten()
        {
            let masked = redact(&text(kept, "text"));
            assert!(
                masked.contains(&text(kept, "kept")),
                "{}: {masked}",
                text(kept, "text")
            );
        }
    }

    #[test]
    fn summary_names_the_working_command_briefly() {
        assert_eq!(
            summarize(
                "cd /w/app-2494 && ls env/service.env 2>/dev/null; DOCKER_HOST=ssh://ci@h timeout 1500 \
                 tools/dev/build.sh app > /tmp/p/build1.log 2>&1; echo rc=$?",
                4,
                32
            ),
            "timeout 1500 tools/dev/build.sh app +1 more"
        );
        assert_eq!(
            summarize(
                "P=/a/b; for i in $(seq 1 9); do sleep 60; done; grep -E ' rc=' $P/runs/status",
                4,
                32
            ),
            "sleep 60 +1 more"
        );
        assert_eq!(
            summarize(
                "timeout 560 bash -c \"until grep -q 'RS6 start' $P/runs/status; do sleep 20; done\"; date -u",
                4,
                32
            ),
            "timeout 560 bash -c 'grep -q RS6 \u{2026} +1 more' +1 more"
        );
        assert_eq!(summarize("date -u; echo done", 4, 32), "date -u +1 more");
        assert_eq!(
            summarize(
                "cat /very/long/path/to/some/evidence/folder/with/a/file.log",
                4,
                32
            ),
            "cat \u{2026}/file.log"
        );
        assert_eq!(summarize("", 4, 32), "");
        assert_eq!(
            summarize_argv(
                &[
                    "bash",
                    "/h/bin/run_queue.sh",
                    "--issue",
                    "2490",
                    "--packet",
                    "/p"
                ],
                2
            ),
            "run_queue.sh --issue 2490 \u{2026}"
        );
        assert_eq!(
            summarize_argv(&["python3", "/h/bin/x.py", "--token", "abc"], 2),
            "x.py --token [redacted]"
        );
    }

    #[test]
    fn a_joiner_a_combining_mark_or_a_connector_beside_a_secret_keeps_it_masked() {
        let cases = [
            "{j}GH_TOKEN=s3cretvalue",
            "{j}--token s3cretvalue",
            "{j}Authorization: s3cretvalue",
            "{j}bearer s3cretvalue",
            "{j}https://user:s3cretvalue@h",
            "{j}curl -u me:s3cretvalue x",
            "{j}curl{j} -u me:s3cretvalue x",
            "{j}sshpass -p s3cretvalue ssh",
            "{j}mysql -ps3cretvalue",
            "x {j}0123456789abcdef0123456789abcdef{j}",
            "run {j}ghp_ABCDEFGHIJKLMNOPQRSTUVWX{j}",
        ];
        for joiner in ["\u{200d}", "\u{200c}", "\u{0301}", "\u{203f}"] {
            for case in cases {
                let text = case.replace("{j}", joiner);
                let masked = redact(&text);
                assert!(masked.contains(MASK), "{text:?}: {masked:?}");
                for secret in ["s3cret", "0123456789abcdef0123", "ghp_A"] {
                    assert!(!masked.contains(secret), "{text:?}: {masked:?}");
                }
                let summary = summarize(&format!("tool {text}"), 12, 200);
                for secret in ["s3cret", "0123456789abcdef0123", "ghp_A"] {
                    assert!(!summary.contains(secret), "{text:?}: {summary:?}");
                }
            }
        }
    }

    #[test]
    fn a_word_character_before_a_name_is_no_boundary() {
        assert_eq!(redact("éTOKEN=abc"), "éTOKEN=abc");
        assert_eq!(redact("Ŝbearer abc"), "Ŝbearer abc");
        assert_eq!(redact("mysqlx -pabc"), "mysqlx -pabc");
        assert_eq!(redact("mysqldump -pabc"), "mysqldump -p[redacted]");
    }

    #[test]
    fn lookbehind_sees_a_multibyte_character_before_the_match() {
        assert_eq!(redact("é--token=abc"), "é--token=[redacted]");
        assert_eq!(redact("x --token abc"), "x --token [redacted]");
        assert_eq!(redact("ä0123456789abcdef0123456789abcdef"), "ä[redacted]");
        assert_eq!(redact("Ŝ--token abc"), "Ŝ--token abc");
        assert_eq!(redact("étoken: abc"), "étoken: abc");
    }
}
