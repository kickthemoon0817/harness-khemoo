"""
Masks secrets in text and reduces command lines to a short, masked summary before they reach the page.
"""
import os
import re
import shlex
from typing import ClassVar


class CommandRedactor:
    """
    Masks secret-looking values and summarises commands.

    `redact` masks, in any text: `KEY=value` and `--flag value` where the name holds TOKEN, KEY, SECRET, PASS,
    AUTH or CRED (any case); `Authorization:` header values and `Bearer` credentials; URL userinfo with a
    password, and any userinfo of an http(s) URL; GitHub, GitLab, Slack, AWS and `sk-` token shapes; hex runs of
    32 characters or more; and base64-like runs of 32 or more mixing upper case, lower case and digits.

    `summarize` redacts first, then keeps only the first command that does real work (leading `cd`, `export`,
    assignment-only and similar steps skipped): its executable's base name and its first few arguments, each
    shortened. No raw command line leaves it.
    """

    MASK: ClassVar[str] = "[redacted]"
    SECRET_WORDS: ClassVar[str] = r"(?:TOKEN|KEY|SECRET|PASS|AUTH|CRED)"
    SECRET_ASSIGNMENT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        rf"(?i)\b([A-Za-z0-9_]*{SECRET_WORDS}[A-Za-z0-9_]*)=(\"[^\"]*\"|'[^']*'|[^\s;&|'\"]+)"
    )
    SECRET_FLAG_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        rf"(?i)(?<![\w-])(--?[A-Za-z0-9-]*{SECRET_WORDS}[A-Za-z0-9-]*)(=|\s+)(\"[^\"]*\"|'[^']*'|[^\s;&|'\"-][^\s;&|'\"]*)"
    )
    AUTHORIZATION_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"(?i)\b(authorization\s*:\s*)(?:(?:bearer|token|basic)\s+)?[^\s'\"]+"
    )
    BEARER_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]+")
    URL_PASSWORD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s@:'\"]+:[^/\s@'\"]*@"
    )
    HTTP_USERINFO_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"(?i)\b(https?://)[^/\s@:'\"]+@")
    TOKEN_SHAPE_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}"
        r"|xox[abposr]-[A-Za-z0-9-]{10,}|sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16})"
    )
    HEX_RUN_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"(?<![0-9A-Za-z])[0-9a-fA-F]{32,}(?![0-9A-Za-z])")
    BASE64_RUN_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"[A-Za-z0-9+/_-]{32,}={0,2}")
    PREAMBLE_COMMANDS: ClassVar[frozenset[str]] = frozenset(
        {"cd", "export", "source", ".", "set", "unset", "true", "false", ":", "local", "umask", "shopt", "break",
         "continue", "exit", "return", "wait", "shift", "trap"}
    )
    LOW_SIGNAL_COMMANDS: ClassVar[frozenset[str]] = frozenset(
        {"ls", "echo", "printf", "date", "pwd", "mkdir", "touch", "test", "[", "cp", "rm", "cat", "wc"}
    )
    LEADING_KEYWORDS: ClassVar[frozenset[str]] = frozenset(
        {"do", "then", "else", "elif", "if", "while", "until", "!", "{", "time", "exec", "eval", "command", "builtin"}
    )
    HEADER_KEYWORDS: ClassVar[frozenset[str]] = frozenset({"for", "select", "case", "done", "fi", "esac", "}"})
    SUBSTITUTION_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\$\(\([^()]*\)\)|\$\([^()]*\)|`[^`]*`")
    SUBSTITUTION_PLACEHOLDER: ClassVar[str] = "__SUBST__"
    ASSIGNMENT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
    PUNCTUATION_CHARACTERS: ClassVar[frozenset[str]] = frozenset("();<>|&")

    @staticmethod
    def redact(text: str) -> str:
        """
        Masks every secret-looking value in a text.

        Args:
            text: Any text: a command line, a log line.

        Returns:
            The text with each secret replaced by MASK.
        """
        mask: str = CommandRedactor.MASK
        masked: str = CommandRedactor.SECRET_ASSIGNMENT_PATTERN.sub(lambda match: f"{match.group(1)}={mask}", text)
        masked = CommandRedactor.SECRET_FLAG_PATTERN.sub(lambda match: f"{match.group(1)}{match.group(2)}{mask}",
                                                         masked)
        masked = CommandRedactor.AUTHORIZATION_PATTERN.sub(lambda match: f"{match.group(1)}{mask}", masked)
        masked = CommandRedactor.BEARER_PATTERN.sub(lambda match: f"{match.group(1)}{mask}", masked)
        masked = CommandRedactor.URL_PASSWORD_PATTERN.sub(lambda match: f"{match.group(1)}{mask}@", masked)
        masked = CommandRedactor.HTTP_USERINFO_PATTERN.sub(lambda match: f"{match.group(1)}{mask}@", masked)
        masked = CommandRedactor.TOKEN_SHAPE_PATTERN.sub(mask, masked)
        masked = CommandRedactor.HEX_RUN_PATTERN.sub(mask, masked)
        return CommandRedactor.BASE64_RUN_PATTERN.sub(
            lambda match: mask if CommandRedactor._looks_random(match.group(0)) else match.group(0), masked)

    @staticmethod
    def summarize(command: str, max_arguments: int = 4, max_argument_chars: int = 32) -> str:
        """
        Reduces a command line to its working command's executable and a few masked, shortened arguments.

        Args:
            command: A shell command line.
            max_arguments: Arguments kept after the executable.
            max_argument_chars: Longest argument kept whole; a longer path keeps its last part.

        Returns:
            `exe arg ...`, `+N more` when further commands follow; empty for an empty command.
        """
        return CommandRedactor._summarize_redacted(CommandRedactor.redact(command), max_arguments, max_argument_chars,
                                                   allow_nested=True)

    @staticmethod
    def _summarize_redacted(text: str, max_arguments: int, max_argument_chars: int, allow_nested: bool) -> str:
        """
        Summarises an already redacted command line.

        Args:
            text: The redacted command line.
            max_arguments: Arguments kept after the executable.
            max_argument_chars: Longest argument kept whole.
            allow_nested: Summarise a `-c` script argument once more, one level deep.

        Returns:
            The summary.
        """
        commands: list[list[str]] = CommandRedactor._simple_commands(text)
        if not commands:
            return ""
        chosen_index: int = 0
        fallback_index: int | None = None
        for index, words in enumerate(commands):
            executable: str = os.path.basename(words[0])
            if executable in CommandRedactor.PREAMBLE_COMMANDS or words[0].startswith("$") \
                    or words[0].startswith(CommandRedactor.SUBSTITUTION_PLACEHOLDER):
                continue
            if fallback_index is None:
                fallback_index = index
            if executable not in CommandRedactor.LOW_SIGNAL_COMMANDS:
                chosen_index = index
                break
        else:
            chosen_index = fallback_index if fallback_index is not None else 0
        words: list[str] = commands[chosen_index]
        arguments: list[str] = []
        index: int = 1
        while index < len(words):
            word: str = words[index]
            following: str = words[index + 1] if index + 1 < len(words) else ""
            if CommandRedactor._is_redirection(word):
                index += 2
                continue
            if word.isdigit() and CommandRedactor._is_redirection(following):
                index += 1
                continue
            if allow_nested and words[index - 1] == "-c" and " " in word.strip():
                nested: str = CommandRedactor._summarize_redacted(word, 2, max_argument_chars, allow_nested=False)
                arguments.append(f"'{nested}'")
            else:
                arguments.append(CommandRedactor._shorten(word, max_argument_chars))
            index += 1
        summary: str = " ".join([os.path.basename(words[0]) or words[0]] + arguments[:max_arguments])
        if len(arguments) > max_arguments:
            summary += " \u2026"
        remaining_commands: int = len(commands) - chosen_index - 1
        if remaining_commands > 0:
            summary += f" +{remaining_commands} more"
        return summary.replace(CommandRedactor.SUBSTITUTION_PLACEHOLDER, "$(\u2026)")

    @staticmethod
    def _simple_commands(text: str) -> list[list[str]]:
        """
        Splits a command line into simple commands: command substitutions collapsed to a placeholder, split at
        control operators, leading assignments and `do`/`then`/`if`/`while`-style keywords dropped, and loop
        headers (`for`, `case`, `done`, ...) left out.

        Args:
            text: The command line.

        Returns:
            Each simple command's words, executable first.
        """
        collapsed: str = text
        while True:
            replaced: str = CommandRedactor.SUBSTITUTION_PATTERN.sub(CommandRedactor.SUBSTITUTION_PLACEHOLDER,
                                                                      collapsed)
            if replaced == collapsed:
                break
            collapsed = replaced
        commands: list[list[str]] = [[]]
        for token in CommandRedactor._tokens(collapsed):
            if CommandRedactor._is_punctuation(token) and not CommandRedactor._is_redirection(token):
                if commands[-1]:
                    commands.append([])
                continue
            commands[-1].append(token)
        simple_commands: list[list[str]] = []
        for words in commands:
            while words and (words[0] in CommandRedactor.LEADING_KEYWORDS
                             or CommandRedactor.ASSIGNMENT_PATTERN.match(words[0])):
                words = words[1:]
            if words and words[0] not in CommandRedactor.HEADER_KEYWORDS:
                simple_commands.append(words)
        return simple_commands

    @staticmethod
    def summarize_argv(argv: tuple[str, ...] | list[str], max_arguments: int = 2) -> str:
        """
        Summarises a process's argv the same way, a script run by an interpreter named by the script.

        Args:
            argv: The process arguments.
            max_arguments: Arguments kept after the executable.

        Returns:
            The masked summary.
        """
        words: list[str] = list(argv)
        if len(words) > 1 and os.path.basename(words[0]) in ("bash", "sh", "python3", "python") \
                and not words[1].startswith("-"):
            words = words[1:]
        return CommandRedactor.summarize(shlex.join(words), max_arguments=max_arguments)

    @staticmethod
    def _tokens(text: str) -> list[str]:
        """
        Splits a command line into words and control operators.

        Args:
            text: The command line.

        Returns:
            Shell words (quotes removed) and operator tokens; plain whitespace split when the quoting is unbalanced.
        """
        lexer: shlex.shlex = shlex.shlex(text, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        try:
            return list(lexer)
        except ValueError:
            return text.split()

    @staticmethod
    def _is_punctuation(token: str) -> bool:
        """
        Args:
            token: A token from _tokens.

        Returns:
            True for an operator token: only `();<>|&` characters.
        """
        return bool(token) and set(token) <= CommandRedactor.PUNCTUATION_CHARACTERS

    @staticmethod
    def _is_redirection(token: str) -> bool:
        """
        Args:
            token: A token from _tokens.

        Returns:
            True for a redirection operator, which belongs to its command and takes the next word as its target.
        """
        return CommandRedactor._is_punctuation(token) and ("<" in token or ">" in token)

    @staticmethod
    def _shorten(word: str, max_chars: int) -> str:
        """
        Args:
            word: One argument.
            max_chars: Longest kept whole.

        Returns:
            The word, a long path cut to its last part, or a long word cut with an ellipsis.
        """
        word = " ".join(word.split())
        if len(word) <= max_chars:
            return word
        if "/" in word.rstrip("/"):
            tail: str = word.rstrip("/").rsplit("/", 1)[1]
            return "…/" + (tail if len(tail) <= max_chars else tail[: max_chars - 1] + "…")
        return word[: max_chars - 1] + "…"

    @staticmethod
    def _looks_random(run: str) -> bool:
        """
        Tells a base64-like secret from a path or a name: it mixes upper case, lower case and digits, with upper
        case at least a tenth of its letters.

        Args:
            run: A run of base64 characters.

        Returns:
            True when the run should be masked.
        """
        uppercase: int = sum(1 for character in run if character.isupper())
        lowercase: int = sum(1 for character in run if character.islower())
        digits: int = sum(1 for character in run if character.isdigit())
        return uppercase > 0 and lowercase > 0 and digits > 0 and uppercase * 10 >= uppercase + lowercase
