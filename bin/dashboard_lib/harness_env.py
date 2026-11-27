"""
Reads named keys from the instance's harness.env without sourcing it.
"""
import re
from pathlib import Path
from typing import ClassVar


class HarnessEnvReader:
    """
    Extracts a fixed set of keys from a shell env file.

    The file is never executed or echoed: each line is matched as a plain `KEY=value` assignment, only the
    requested keys are kept, and values are returned verbatim (no `$` expansion). The last assignment of a key
    wins, as it would when the shell sources the file.
    """

    ASSIGNMENT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$")

    @staticmethod
    def read_keys(env_path: Path, wanted_keys: frozenset[str]) -> dict[str, str]:
        """
        Returns the wanted keys that the file assigns.

        Args:
            env_path: The env file; a missing or unreadable file yields an empty mapping.
            wanted_keys: The only keys to extract.

        Returns:
            Key to unquoted value, for each wanted key the file assigns.
        """
        try:
            text: str = env_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return {}
        values: dict[str, str] = {}
        for line in text.splitlines():
            match: re.Match[str] | None = HarnessEnvReader.ASSIGNMENT_PATTERN.match(line)
            if match is None or match.group(1) not in wanted_keys:
                continue
            values[match.group(1)] = HarnessEnvReader._unquote(match.group(2))
        return values

    @staticmethod
    def _unquote(raw_value: str) -> str:
        """
        Strips shell quoting and a trailing comment from an assignment's right-hand side.

        Args:
            raw_value: Everything after the `=`.

        Returns:
            The value as the shell would assign it, without expansion.
        """
        stripped: str = raw_value.strip()
        if stripped[:1] in ("'", '"'):
            quote: str = stripped[0]
            closing_index: int = stripped.find(quote, 1)
            return stripped[1:closing_index] if closing_index > 0 else stripped[1:]
        comment_match: re.Match[str] | None = re.search(r"\s#", stripped)
        return stripped[: comment_match.start()].rstrip() if comment_match else stripped
