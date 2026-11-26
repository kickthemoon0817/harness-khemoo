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

    Secret-looking values: `KEY=value` and `--flag value` / `--flag=value` where the name holds TOKEN, KEY, SECRET,
    PASS, AUTH, CRED, COOKIE or SESSION (any case); the value of a header whose name holds one of them
    (`Authorization:`, `Proxy-Authorization:`, `X-Api-Key:`, `Cookie:`) and `Bearer` credentials; URL userinfo
    with a password, and any userinfo of an http(s) URL; GitHub, GitLab, Slack, AWS and `sk-` token shapes; hex
    runs of 32 characters or more; and base64-like runs of 32 or more mixing upper case, lower case and digits.

    `redact` masks them in free text (a log line) that is shown as it is. `summarize` works on what the shell
    would see: it tokenizes first, the way the shell dequotes, and masks each dequoted word. A secret-named flag
    masks every word after it up to the next flag or operator; a secret-named header word masks the words after
    it up to the next flag or operator but `;`, which separates a cookie's pairs, an auth scheme right after it
    kept. A word that is itself a script (a `-c` argument, any word holding spaces or quotes) is tokenized and
    masked again, to a fixed depth past which it is masked whole. When the quoting does not parse, everything
    from the failure on is masked. Each command's masked words then go through `redact` once more, joined, so a
    pattern that spans words still applies. Only then does it shorten the result to the working command's
    executable and a few arguments. No raw command line leaves it.
    """

    MASK: ClassVar[str] = "[redacted]"
    SECRET_WORDS: ClassVar[str] = r"(?:TOKEN|KEY|SECRET|PASS|AUTH|CRED|COOKIE|SESSION)"
    SECRET_NAME_PATTERN: ClassVar[re.Pattern[str]] = re.compile(rf"(?i){SECRET_WORDS}")
    SECRET_FLAG_WORD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(rf"(?i)^--?[A-Za-z0-9-]*{SECRET_WORDS}[A-Za-z0-9-]*$")
    NAME_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^(?:[A-Za-z_][A-Za-z0-9_]*|--?[A-Za-z0-9][A-Za-z0-9-]*)$")
    ESCAPED_OR_QUOTED_VALUE: ClassVar[str] = r"\\\"[^\"]*?\\\"|\\'[^']*?\\'|\"[^\"]*\"|'[^']*'"
    SECRET_ASSIGNMENT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        rf"(?i)\b([A-Za-z0-9_]*{SECRET_WORDS}[A-Za-z0-9_]*)=({ESCAPED_OR_QUOTED_VALUE}|[^\s;&|]+)"
    )
    SECRET_FLAG_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        rf"(?i)(?<![\w-])(--?[A-Za-z0-9-]*{SECRET_WORDS}[A-Za-z0-9-]*)(=|\s+)"
        rf"({ESCAPED_OR_QUOTED_VALUE}|(?!--?[A-Za-z0-9-]*{SECRET_WORDS}[A-Za-z0-9-]*(?:[\s=]|$))[^\s;&|][^\s;&|]*)"
    )
    SECRET_HEADER_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        rf"(?i)(?<![\w-])([A-Za-z0-9_-]*{SECRET_WORDS}[A-Za-z0-9_-]*\s*:\s*)(?:(?:bearer|token|basic|digest)\s+)?"
        r"(?!//)[^\s'\"]+"
    )
    HEADER_WORD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^([A-Za-z0-9_-]+):(\S*)$")
    FLAG_WORD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^--?[A-Za-z][A-Za-z0-9_-]*(?:=.*)?$")
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
    AUTH_SCHEMES: ClassVar[frozenset[str]] = frozenset({"bearer", "token", "basic", "digest"})
    SCRIPT_CHARACTERS: ClassVar[frozenset[str]] = frozenset(" \t\n'\"\\;|&")
    MAX_SCRIPT_DEPTH: ClassVar[int] = 4
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
        Masks every secret-looking value in free text shown as it is, quotes and escapes included.

        Args:
            text: A log line or any other text that is not dequoted before display.

        Returns:
            The text with each secret replaced by MASK.
        """
        mask: str = CommandRedactor.MASK
        masked: str = CommandRedactor.SECRET_ASSIGNMENT_PATTERN.sub(lambda match: f"{match.group(1)}={mask}", text)
        masked = CommandRedactor.SECRET_FLAG_PATTERN.sub(lambda match: f"{match.group(1)}{match.group(2)}{mask}",
                                                         masked)
        masked = CommandRedactor.SECRET_HEADER_PATTERN.sub(lambda match: f"{match.group(1)}{mask}", masked)
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
        return CommandRedactor._summarize_words(CommandRedactor.redact_command(command), max_arguments,
                                                max_argument_chars, allow_nested=True)

    @staticmethod
    def summarize_argv(argv: tuple[str, ...] | list[str], max_arguments: int = 2) -> str:
        """
        Summarises a process's argv, already split into words, the same way; a script run by an interpreter is
        named by the script.

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
        return CommandRedactor._summarize_words(CommandRedactor._redact_words(words, depth=0), max_arguments, 32,
                                                allow_nested=True)

    @staticmethod
    def redact_command(command: str, depth: int = 0) -> list[str]:
        """
        Tokenizes a command line as the shell would and masks each dequoted word.

        Args:
            command: A shell command line.
            depth: How many scripts deep this command sits.

        Returns:
            Its masked words and operators; MASK stands for everything from a quoting failure on.
        """
        collapsed: str = command
        while True:
            replaced: str = CommandRedactor.SUBSTITUTION_PATTERN.sub(CommandRedactor.SUBSTITUTION_PLACEHOLDER,
                                                                      collapsed)
            if replaced == collapsed:
                break
            collapsed = replaced
        words, parsed_whole = CommandRedactor._split_words(collapsed)
        redacted: list[str] = CommandRedactor._redact_words(words, depth)
        if not parsed_whole:
            redacted.append(CommandRedactor.MASK)
        joined: str = "\n".join(redacted)
        backstopped: str = CommandRedactor.redact(joined)
        if backstopped != joined:
            redacted = [word for word in backstopped.split("\n") if word]
        return redacted

    @staticmethod
    def _redact_words(words: list[str], depth: int) -> list[str]:
        """
        Masks a list of dequoted words.

        A secret-named flag (`--token`) masks every word after it up to the next flag or operator. The first word
        after it is masked whatever its shape, since a secret value may start with a dash, unless that word is
        itself a secret-named flag, which is then taken as a flag: `--token --password secret` masks `secret`, and
        `--token -abc` masks `-abc`. A header word whose name
        holds a secret word (`Authorization:`, `X-Api-Key:`, `Cookie:`), `Bearer`, or `Authorization:Bearer`
        masks the words after it up to the next flag or operator but `;`, keeping an auth scheme that comes right
        after a bare header name; a header word carrying its value (`X-Api-Key:abc`) loses that value too. Every
        other word is masked on its own.

        Args:
            words: Dequoted words and operators.
            depth: How many scripts deep they sit.

        Returns:
            The masked words.
        """
        redacted: list[str] = []
        masking: str = ""
        scheme_may_follow: bool = False
        value_owed: bool = False
        for word in words:
            if CommandRedactor._is_punctuation(word):
                redacted.append(word)
                if not (masking == "header" and word == ";"):
                    masking = ""
                    value_owed = False
                scheme_may_follow = False
                continue
            if masking:
                if value_owed:
                    taken_as_flag = CommandRedactor.SECRET_FLAG_WORD_PATTERN.match(word) is not None
                else:
                    taken_as_flag = CommandRedactor.FLAG_WORD_PATTERN.match(word) is not None
                if not taken_as_flag:
                    if scheme_may_follow and word.lower() in CommandRedactor.AUTH_SCHEMES:
                        redacted.append(word)
                    else:
                        redacted.append(CommandRedactor.MASK)
                        value_owed = False
                    scheme_may_follow = False
                    continue
            masking = ""
            scheme_may_follow = False
            value_owed = False
            if CommandRedactor.SECRET_FLAG_WORD_PATTERN.match(word):
                redacted.append(word)
                masking = "flag"
                value_owed = True
                continue
            if word.lower() == "bearer":
                redacted.append(word)
                masking = "header"
                value_owed = True
                continue
            header_match: re.Match[str] | None = CommandRedactor.HEADER_WORD_PATTERN.match(word)
            if header_match and CommandRedactor.SECRET_NAME_PATTERN.search(header_match.group(1)) \
                    and not header_match.group(2).startswith("//"):
                name, value = header_match.group(1), header_match.group(2)
                masking = "header"
                if not value:
                    redacted.append(word)
                    scheme_may_follow = True
                    value_owed = True
                elif value.lower() in CommandRedactor.AUTH_SCHEMES:
                    redacted.append(word)
                    value_owed = True
                else:
                    redacted.append(f"{name}:{CommandRedactor.MASK}")
                continue
            redacted.append(CommandRedactor._redact_word(word, depth))
        return redacted

    @staticmethod
    def _redact_word(word: str, depth: int) -> str:
        """
        Masks one dequoted word: a secret-named `name=value` loses its whole value, a word that is a script is
        tokenized and masked in turn, and any other word goes through the free-text masks.

        Args:
            word: One dequoted word.
            depth: How many scripts deep it sits.

        Returns:
            The masked word.
        """
        name, equals, _ = word.partition("=")
        if equals and CommandRedactor.NAME_PATTERN.match(name) and CommandRedactor.SECRET_NAME_PATTERN.search(name):
            return f"{name}={CommandRedactor.MASK}"
        if any(character in CommandRedactor.SCRIPT_CHARACTERS for character in word):
            if depth >= CommandRedactor.MAX_SCRIPT_DEPTH:
                return CommandRedactor.MASK
            return " ".join(CommandRedactor.redact_command(word, depth + 1))
        return CommandRedactor.redact(word)

    @staticmethod
    def _split_words(text: str) -> tuple[list[str], bool]:
        """
        Splits a command line into dequoted words and control operators, as the shell does.

        Args:
            text: The command line.

        Returns:
            The words read, and whether the whole text parsed; on a quoting failure, only the words before it.
        """
        lexer: shlex.shlex = shlex.shlex(text, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        words: list[str] = []
        try:
            while True:
                token: str | None = lexer.get_token()
                if token is None or token == lexer.eof:
                    return words, True
                words.append(token)
        except ValueError:
            return words, False

    @staticmethod
    def _summarize_words(words: list[str], max_arguments: int, max_argument_chars: int, allow_nested: bool) -> str:
        """
        Summarises masked words: picks the working command and keeps its executable and first arguments.

        Args:
            words: Masked words and operators from redact_command.
            max_arguments: Arguments kept after the executable.
            max_argument_chars: Longest argument kept whole.
            allow_nested: Summarise a `-c` script argument once more, one level deep.

        Returns:
            The summary.
        """
        commands: list[list[str]] = CommandRedactor._simple_commands(words)
        if not commands:
            return ""
        chosen_index: int = 0
        fallback_index: int | None = None
        for index, command_words in enumerate(commands):
            executable: str = os.path.basename(command_words[0])
            if executable in CommandRedactor.PREAMBLE_COMMANDS or command_words[0].startswith("$") \
                    or command_words[0].startswith(CommandRedactor.SUBSTITUTION_PLACEHOLDER):
                continue
            if fallback_index is None:
                fallback_index = index
            if executable not in CommandRedactor.LOW_SIGNAL_COMMANDS:
                chosen_index = index
                break
        else:
            chosen_index = fallback_index if fallback_index is not None else 0
        chosen: list[str] = commands[chosen_index]
        arguments: list[str] = []
        index: int = 1
        while index < len(chosen):
            word: str = chosen[index]
            following: str = chosen[index + 1] if index + 1 < len(chosen) else ""
            if CommandRedactor._is_redirection(word):
                index += 2
                continue
            if word.isdigit() and CommandRedactor._is_redirection(following):
                index += 1
                continue
            if allow_nested and chosen[index - 1] == "-c" and " " in word.strip():
                nested: str = CommandRedactor._summarize_words(CommandRedactor.redact_command(word), 2,
                                                               max_argument_chars, allow_nested=False)
                arguments.append(f"'{nested}'")
            else:
                arguments.append(CommandRedactor._shorten(word, max_argument_chars))
            index += 1
        summary: str = " ".join([os.path.basename(chosen[0]) or chosen[0]] + arguments[:max_arguments])
        if len(arguments) > max_arguments:
            summary += " …"
        remaining_commands: int = len(commands) - chosen_index - 1
        if remaining_commands > 0:
            summary += f" +{remaining_commands} more"
        return summary.replace(CommandRedactor.SUBSTITUTION_PLACEHOLDER, "$(…)")

    @staticmethod
    def _simple_commands(words: list[str]) -> list[list[str]]:
        """
        Groups masked words into simple commands: split at control operators, leading assignments and
        `do`/`then`/`if`/`while`-style keywords dropped, and loop headers (`for`, `case`, `done`, ...) left out.

        Args:
            words: Masked words and operators.

        Returns:
            Each simple command's words, executable first.
        """
        commands: list[list[str]] = [[]]
        for word in words:
            if CommandRedactor._is_punctuation(word) and not CommandRedactor._is_redirection(word):
                if commands[-1]:
                    commands.append([])
                continue
            commands[-1].append(word)
        simple_commands: list[list[str]] = []
        for command_words in commands:
            while command_words and (command_words[0] in CommandRedactor.LEADING_KEYWORDS
                                     or CommandRedactor.ASSIGNMENT_PATTERN.match(command_words[0])):
                command_words = command_words[1:]
            if command_words and command_words[0] not in CommandRedactor.HEADER_KEYWORDS:
                simple_commands.append(command_words)
        return simple_commands

    @staticmethod
    def _is_punctuation(token: str) -> bool:
        """
        Args:
            token: A word or operator.

        Returns:
            True for an operator token: only `();<>|&` characters.
        """
        return bool(token) and set(token) <= CommandRedactor.PUNCTUATION_CHARACTERS

    @staticmethod
    def _is_redirection(token: str) -> bool:
        """
        Args:
            token: A word or operator.

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
