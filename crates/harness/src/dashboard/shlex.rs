//! POSIX shell word splitting, and its quoting.
//!
//! The redactor and the instance's command keys both need words as a POSIX shell dequotes them,
//! `#` starting a comment for the redactor, and, for the redactor, control operators (`;`, `&&`,
//! `|`, `>`...) as words of their own.

const PUNCTUATION: &str = "();<>|&";
const WHITESPACE: &str = " \t\r\n";

/// Why a command line does not split.
#[derive(Debug, PartialEq, Eq)]
pub enum SplitError {
    NoClosingQuotation,
    NoEscapedCharacter,
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NoClosingQuotation => "No closing quotation",
            Self::NoEscapedCharacter => "No escaped character",
        })
    }
}

/// The words read so far, and the error that stopped the split, if one did.
pub struct Split {
    pub words: Vec<String>,
    pub error: Option<SplitError>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Whitespace,
    Word,
    Punctuation,
    Quote(char),
    Escape(char),
    End,
}

struct Lexer {
    characters: Vec<char>,
    position: usize,
    pushed_back: Option<char>,
    punctuation: bool,
    comments: bool,
    state: State,
}

impl Lexer {
    fn next_char(&mut self) -> Option<char> {
        if let Some(character) = self.pushed_back.take() {
            return Some(character);
        }
        let character = self.characters.get(self.position).copied();
        if character.is_some() {
            self.position += 1;
        }
        character
    }

    fn skip_line(&mut self) {
        while let Some(character) = self.characters.get(self.position) {
            self.position += 1;
            if *character == '\n' {
                break;
            }
        }
    }

    fn is_punctuation(&self, character: char) -> bool {
        self.punctuation && PUNCTUATION.contains(character)
    }

    fn is_comment(&self, character: char) -> bool {
        self.comments && character == '#'
    }

    /// One token, or none at the end; mirrors `shlex.read_token` with `whitespace_split` set.
    fn read_token(&mut self) -> Result<Option<String>, SplitError> {
        let mut token = String::new();
        let mut quoted = false;
        let mut escaped_state = State::Word;
        loop {
            let next = self.next_char();
            match self.state {
                State::End => return Ok(None),
                State::Whitespace => match next {
                    None => {
                        self.state = State::End;
                        break;
                    }
                    Some(character) if WHITESPACE.contains(character) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some(character) if self.is_comment(character) => self.skip_line(),
                    Some('\\') => {
                        escaped_state = State::Word;
                        self.state = State::Escape('\\');
                    }
                    Some(character) if self.is_punctuation(character) => {
                        token.push(character);
                        self.state = State::Punctuation;
                    }
                    Some(character @ ('\'' | '"')) => self.state = State::Quote(character),
                    Some(character) => {
                        token.push(character);
                        self.state = State::Word;
                    }
                },
                State::Quote(quote) => {
                    quoted = true;
                    match next {
                        None => return Err(SplitError::NoClosingQuotation),
                        Some(character) if character == quote => self.state = State::Word,
                        Some('\\') if quote == '"' => {
                            escaped_state = State::Quote(quote);
                            self.state = State::Escape('\\');
                        }
                        Some(character) => token.push(character),
                    }
                }
                State::Escape(escape) => {
                    let Some(character) = next else {
                        return Err(SplitError::NoEscapedCharacter);
                    };
                    // Inside double quotes only the quote itself or the escape can be escaped.
                    if let State::Quote(quote) = escaped_state
                        && character != escape
                        && character != quote
                    {
                        token.push(escape);
                    }
                    token.push(character);
                    self.state = escaped_state;
                }
                State::Word | State::Punctuation => match next {
                    None => {
                        self.state = State::End;
                        break;
                    }
                    Some(character) if WHITESPACE.contains(character) => {
                        self.state = State::Whitespace;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some(character) if self.is_comment(character) => {
                        self.skip_line();
                        self.state = State::Whitespace;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some(character) if self.state == State::Punctuation => {
                        if self.is_punctuation(character) {
                            token.push(character);
                        } else {
                            if !WHITESPACE.contains(character) {
                                self.pushed_back = Some(character);
                            }
                            self.state = State::Whitespace;
                            break;
                        }
                    }
                    Some(character @ ('\'' | '"')) => self.state = State::Quote(character),
                    Some('\\') => {
                        escaped_state = State::Word;
                        self.state = State::Escape('\\');
                    }
                    Some(character) if !self.is_punctuation(character) => token.push(character),
                    Some(character) => {
                        self.pushed_back = Some(character);
                        self.state = State::Whitespace;
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                },
            }
        }
        if token.is_empty() && !quoted {
            return Ok(None);
        }
        Ok(Some(token))
    }
}

fn split_with(text: &str, punctuation: bool, comments: bool) -> Split {
    let mut lexer = Lexer {
        characters: text.chars().collect(),
        position: 0,
        pushed_back: None,
        punctuation,
        comments,
        state: State::Whitespace,
    };
    let mut words = Vec::new();
    loop {
        match lexer.read_token() {
            Ok(Some(word)) => words.push(word),
            Ok(None) => return Split { words, error: None },
            Err(error) => {
                return Split {
                    words,
                    error: Some(error),
                };
            }
        }
    }
}

/// Splits a command line into dequoted words and control operators, `#` starting a comment.
pub fn split_command(text: &str) -> Split {
    split_with(text, true, true)
}

/// Splits a configured command into its argv words; no operators and no comments.
pub fn split(text: &str) -> Result<Vec<String>, SplitError> {
    let Split { words, error } = split_with(text, false, false);
    match error {
        Some(error) => Err(error),
        None => Ok(words),
    }
}

/// One word quoted for a POSIX shell, left bare when it holds only safe characters.
pub fn quote(word: &str) -> String {
    if word.is_empty() {
        return "''".to_owned();
    }
    if word
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"%+,-./:=@_".contains(&byte))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

/// Words joined into a command line a POSIX shell splits back into the same words.
pub fn join<S: AsRef<str>>(words: &[S]) -> String {
    words
        .iter()
        .map(|word| quote(word.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        split_command(text).words
    }

    #[test]
    fn operators_are_words() {
        assert_eq!(
            words("a&&b|c >f 2>&1;d"),
            ["a", "&&", "b", "|", "c", ">", "f", "2", ">&", "1", ";", "d"]
        );
        assert_eq!(
            words("echo 'a b' \"c\\\"d\" e\\ f"),
            ["echo", "a b", "c\"d", "e f"]
        );
        assert_eq!(words("tool # comment\nnext"), ["tool", "next"]);
        assert_eq!(words("x ''"), ["x", ""]);
    }

    #[test]
    fn quoting_failures_keep_the_words_before() {
        let result = split_command("tool --name \"a b");
        assert_eq!(result.words, ["tool", "--name"]);
        assert_eq!(result.error, Some(SplitError::NoClosingQuotation));
    }

    #[test]
    fn split_and_join_round_trip() {
        let argv = ["bash", "-c", "it's $(id)", "", "plain"];
        assert_eq!(
            split(&join(&argv)).ok().as_deref(),
            Some(&argv.map(str::to_owned)[..])
        );
        assert_eq!(
            split("a #b").ok(),
            Some(vec!["a".to_owned(), "#b".to_owned()])
        );
    }
}
