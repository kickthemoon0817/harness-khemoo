#!/usr/bin/env python3
"""Exercises bin/evidence_trim.py on a scratch evidence tree."""
import os
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TOOL = os.path.join(HERE, "bin", "evidence_trim.py")
OLD = time.time() - 3 * 86400


def write(path, size=10, old=True):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(b"1," * (size // 2))
    if old:
        os.utime(path, (OLD, OLD))


class EvidenceTrim(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.ev = self.tmp.name
        self.env = dict(os.environ, EVIDENCE_ROOT=self.ev, EVIDENCE_OPEN_ISSUES="2000",
                        EVIDENCE_RECENT_HEADS="aaaaaaa")
        e = lambda *p: os.path.join(self.ev, *p)
        self.closed = e("2026-09-20-1000-x")
        write(e(self.closed, "runs", "RS4-x", "frames", "video", "RS4_C1_v000001.png"))
        write(e(self.closed, "runs", "RS4-x", "frames", "RS4_C1.mp4"))
        write(e(self.closed, "runs", "RS4-x", "frames", "video", "RS3_C2_v000001.png"))  # no MP4
        write(e(self.closed, "runs", "RS4-x", "frames", "RS4_C1_M0_t1_u1.depth.npy"))
        write(e(self.closed, "runs", "RS4-x", "wrench-trace.csv"), size=30_000_000)
        write(e(self.closed, "runs", "RS4-x", "small.csv"))
        write(e(self.closed, "runs", "RS4-x-void1", "kit.log"))
        write(e(self.closed, "INDEX.md"))
        self.open_ = e("2026-09-20-2000-y")
        write(e(self.open_, "runs", "a", "x.npy"))
        write(e(self.open_, "runs", "a", "wrench-trace.csv"), size=30_000_000)
        write(e(self.open_, "runs", "a-void2", "kit.log"))
        self.fresh = e("2026-09-20-1001-z")
        write(e(self.fresh, "a.npy"), old=False)
        self.base = e("2026-09-19-baseline")
        write(e(self.base, "a.npy"))
        with open(e("BASELINES.md"), "w") as f:
            f.write("- `2026-09-19-baseline/`\n")
        self.old_parent = e("parent-bbbbbbb1")
        write(e(self.old_parent, "RS1-p", "documents.jsonl"))
        write(e(self.old_parent, "chain.txt"))
        self.new_parent = e("parent-aaaaaaa1")
        write(e(self.new_parent, "RS1-p", "documents.jsonl"))

    def tearDown(self):
        self.tmp.cleanup()

    def run_tool(self, *args):
        return subprocess.run([sys.executable, TOOL, *args], env=self.env, capture_output=True, text=True, check=True).stdout

    def exists(self, *parts):
        return os.path.exists(os.path.join(*parts))

    def test_dry_run_changes_nothing(self):
        out = self.run_tool()
        self.assertIn("(dry run)", out)
        self.assertTrue(self.exists(self.closed, "runs", "RS4-x", "frames", "RS4_C1_M0_t1_u1.depth.npy"))

    def test_apply(self):
        self.run_tool("--apply")
        frames = os.path.join(self.closed, "runs", "RS4-x", "frames")
        self.assertFalse(self.exists(frames, "video", "RS4_C1_v000001.png"), "a frame its MP4 holds goes")
        self.assertTrue(self.exists(frames, "video", "RS3_C2_v000001.png"), "a frame with no MP4 stays")
        self.assertTrue(self.exists(frames, "RS4_C1.mp4"))
        self.assertFalse(self.exists(frames, "RS4_C1_M0_t1_u1.depth.npy"), "a closed issue's AOV goes")
        self.assertTrue(self.exists(self.closed, "runs", "RS4-x", "wrench-trace.csv.zst"), "a closed trace is compressed")
        self.assertFalse(self.exists(self.closed, "runs", "RS4-x", "wrench-trace.csv"))
        self.assertTrue(self.exists(self.closed, "runs", "RS4-x", "small.csv"), "a small csv stays")
        self.assertFalse(self.exists(self.closed, "runs", "RS4-x-void1", "kit.log"), "a void run goes")
        self.assertTrue(self.exists(self.closed, "INDEX.md"))
        self.assertTrue(self.exists(self.closed, "CLEANED.txt"))
        self.assertTrue(self.exists(self.open_, "runs", "a", "x.npy"), "an open issue keeps its AOVs")
        self.assertTrue(self.exists(self.open_, "runs", "a", "wrench-trace.csv"), "and its traces")
        self.assertFalse(self.exists(self.open_, "runs", "a-void2", "kit.log"), "but not its void runs")
        self.assertTrue(self.exists(self.fresh, "a.npy"), "a packet touched recently is left alone")
        self.assertTrue(self.exists(self.base, "a.npy"), "a baseline is never touched")
        self.assertFalse(self.exists(self.old_parent, "RS1-p", "documents.jsonl"), "an old parent's bulk goes")
        self.assertTrue(self.exists(self.old_parent, "chain.txt"), "its small text stays")
        self.assertTrue(self.exists(self.new_parent, "RS1-p", "documents.jsonl"), "a recent parent stays whole")


if __name__ == "__main__":
    unittest.main()
