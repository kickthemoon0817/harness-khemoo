#!/usr/bin/env python3
"""Keeps the evidence folder from filling the disk with what a packet no longer needs.

Per packet (a folder of the evidence root), nothing touched in the last FRESH_HOURS is changed:
  A  video frame PNGs (`frames/video/<name>_v<n>.png`) go once `frames/<name>.mp4` holds them;
  B  .npy AOVs go once the packet's issue is closed, or its parent base is no longer a recent head;
  C  void runs (a path part ending `-void<n>`) go;
  D  a parent folder of a base that is no longer a recent head keeps only its small text files;
  E  wrench traces over 20 MB of a closed issue are compressed with zstd (`zstd -dc` reads them).
An open issue's packet keeps everything but A and C. Packets named in BASELINES.md are never touched.
Each changed packet gets CLEANED.txt saying what went.
    evidence_trim.py            dry run
    evidence_trim.py --apply    acts
"""
import argparse
import collections
import os
import re
import subprocess
import sys
import time

EVIDENCE = os.environ.get("EVIDENCE_ROOT", os.path.expanduser("~/tmp_workspace/artifacts/manure-plan/evidence"))
REPO = os.environ.get("MAIN_REPO", os.path.expanduser("~/tmp_workspace/isaac_sim"))
GH_REPO = os.environ.get("GH_REPO", "MaumAI-Company/isaac_sim")
KEEP_HEADS = int(os.environ.get("KEEP_HEADS", "10"))
FRESH_HOURS = float(os.environ.get("FRESH_HOURS", "24"))
TRACE_MIN_BYTES = 20_000_000
SMALL_TEXT = ("INDEX", "chain", "status", "CLEANED", "AOV_TRIMMED")


def open_issues():
    if "EVIDENCE_OPEN_ISSUES" in os.environ:
        return {int(x) for x in os.environ["EVIDENCE_OPEN_ISSUES"].split()}
    out = subprocess.run(["gh", "issue", "list", "-R", GH_REPO, "--state", "open", "--limit", "500",
                          "--json", "number", "--jq", ".[].number"], capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit("gh unavailable; nothing trimmed")
    return {int(x) for x in out.stdout.split()}


def recent_heads():
    if "EVIDENCE_RECENT_HEADS" in os.environ:
        return set(os.environ["EVIDENCE_RECENT_HEADS"].split())
    out = subprocess.run(["git", "-C", REPO, "rev-list", "--first-parent", "-n", str(KEEP_HEADS), "origin/ai/manure-mpm"],
                         capture_output=True, text=True)
    return {h[:7] for h in out.stdout.split()}


def baselines():
    try:
        text = open(os.path.join(EVIDENCE, "BASELINES.md")).read()
    except OSError:
        return set()
    return {m.strip("`/") for m in re.findall(r"`\d{4}-\d{2}-\d{2}-[^`/]+/`", text)}


def packet_state(name, open_, recent, kept):
    if name in kept:
        return "baseline"
    m = re.match(r"^\d{4}-\d{2}-\d{2}-(\d{3,5})-", name)
    if m:
        return "open" if int(m.group(1)) in open_ else "closed"
    m = re.match(r"^(parent|base)-([0-9a-f]{7,})", name)
    if m:
        return "parent-recent" if m.group(2)[:7] in recent else "parent-old"
    return "other"


def touched_since(path, cutoff):
    for root, _, files in os.walk(path):
        for f in files:
            try:
                if os.path.getmtime(os.path.join(root, f)) > cutoff:
                    return True
            except OSError:
                pass
    return False


def plan(evidence, open_, recent, kept, now):
    """Yields (category, action, path) for every file the rules reach."""
    cutoff = now - FRESH_HOURS * 3600
    for name in sorted(os.listdir(evidence)):
        packet = os.path.join(evidence, name)
        if not os.path.isdir(packet):
            continue
        state = packet_state(name, open_, recent, kept)
        if state == "baseline" or touched_since(packet, cutoff):
            continue
        for root, _, files in os.walk(packet):
            rel = os.path.relpath(root, evidence) + "/"
            if os.path.basename(root) == "video":
                for f in files:
                    m = re.match(r"^(.*)_v\d+\.png$", f)
                    mp4 = os.path.join(os.path.dirname(root), (m.group(1) + ".mp4") if m else "")
                    if m and os.path.isfile(mp4) and os.path.getsize(mp4) > 0:
                        yield name, "A-video-frames", "rm", os.path.join(root, f)
                continue
            void = re.search(r"-void\d*/", rel) is not None
            for f in files:
                path = os.path.join(root, f)
                if void:
                    yield name, "C-void-runs", "rm", path
                elif state in ("open", "other", "parent-recent"):
                    continue
                elif f.endswith(".npy"):
                    yield name, "B-npy-aovs", "rm", path
                elif state == "parent-old" and re.search(r"\.(csv|jsonl|png|json|mp4)$", f) and not f.startswith(SMALL_TEXT):
                    yield name, "D-old-parents", "rm", path
                elif state == "closed" and f.endswith(".csv") and os.path.getsize(path) > TRACE_MIN_BYTES:
                    yield name, "E-traces-zstd", "zstd", path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true")
    args = ap.parse_args()
    now = time.time()
    totals = collections.defaultdict(lambda: [0, 0])
    changed = set()
    for packet, cat, how, path in plan(EVIDENCE, open_issues(), recent_heads(), baselines(), now):
        size = os.path.getsize(path)
        totals[cat][0] += size
        totals[cat][1] += 1
        changed.add(packet)
        if not args.apply:
            continue
        if how == "rm":
            os.remove(path)
        else:
            subprocess.run(["nice", "-n", "19", "zstd", "-q", "-3", "-T4", "--rm", path], check=False)
    if args.apply:
        stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now))
        for packet in changed:
            with open(os.path.join(EVIDENCE, packet, "CLEANED.txt"), "a") as note:
                note.write(f"{stamp}: evidence_trim.py removed what this packet no longer needs (video frames its MP4 holds, "
                           ".npy AOVs, void runs, an old parent's bulk) and compressed wrench traces over 20 MB with zstd "
                           "(`zstd -dc <file>.csv.zst`). INDEX.md and checks/ are unchanged; re-run the arm script to regenerate.\n")
    for cat, (size, count) in sorted(totals.items()):
        print(f"{cat:16} {size / 1e9:7.1f} GB {count:7d} files")
    print(f"{len(changed)} packets{'' if args.apply else ' (dry run)'}")


if __name__ == "__main__":
    main()
