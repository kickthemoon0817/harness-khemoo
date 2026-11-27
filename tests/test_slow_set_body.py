#!/usr/bin/env python3
"""Checks that the issue bin/slow_set.sh files is written to prompts/public-record.md.

The script's body and trace are printf format strings. This test renders each with sample values and
runs bin/public_record_lint.py over the result: the body must lint clean as an issue, and the trace,
which carries the host's log path, must lint clean as a comment because that path sits inside its
Harness trace block.
"""
import os
import re
import subprocess
import unittest

HERE: str = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT: str = os.path.join(HERE, "bin", "slow_set.sh")
LINT: str = os.path.join(HERE, "bin", "public_record_lint.py")


class SlowSetBody(unittest.TestCase):
    """Renders the script's printf templates and lints them."""

    def setUp(self) -> None:
        with open(SCRIPT, encoding="utf-8") as handle:
            self.script: str = handle.read()

    def test_body_lints_clean_as_an_issue(self) -> None:
        body: str = self._render("body", (
            "fast set", "ai/manure-mpm", "8f4e54b3", "test_manure_spec :: the zone-sync window reports what it "
            "reported before  [fails alone: regression]\nERROR: CHECK( keys == pinned ) is NOT correct!",
            " `4f04f3f1`", "- 4dcc39bf perf: hold the return walk's body and borne reads", " --fast", "8f4e54b3",
        ))
        self.assertIn("## Summary", body)
        self.assertIn("## Ask", body)
        self.assertEqual(self._lint(body, "issue"), 0, body)

    def test_trace_lints_clean_as_a_comment(self) -> None:
        trace: str = self._render("trace", ("/home/someone/harness/state/fast-set/logs/20261103T091500Z-8f4e54b3",
                                            "fast"))
        self.assertIn("Harness trace", trace)
        self.assertEqual(self._lint(trace, "comment"), 0, trace)

    def _render(self, variable: str, values: tuple[str, ...]) -> str:
        match: re.Match[str] | None = re.search(rf"^{variable}=\$\(printf '(.*?)' ", self.script,
                                                re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(match, f"{variable}=$(printf '...' not found in bin/slow_set.sh")
        template: str = match.group(1).replace("%b", "%s").replace("\\n", "\n")
        self.assertEqual(template.count("%s"), len(values), "the sample values must match the format's fields")
        return template % values

    @staticmethod
    def _lint(text: str, kind: str) -> int:
        result: subprocess.CompletedProcess[str] = subprocess.run(
            ["python3", LINT, "--kind", kind, "-"], input=text, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            print(result.stdout)
        return result.returncode


if __name__ == "__main__":
    unittest.main()
