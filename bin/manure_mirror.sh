#!/usr/bin/env bash
# Mirror isaac_sim's extensions/env/worv.env.manure into its own repository,
# MaumAI-Company/worv.env.manure, until the extension becomes a submodule.
#
# The mirror is the subtree split of origin/ai/manure-mpm with every
# "Co-Authored-By:" trailer dropped from the commit messages (owner,
# 2026-11-03). Both steps are deterministic, so a re-run reproduces the
# commits already pushed and only appends new ones; the push is refused unless
# it is a fast-forward of the remote main.
#
# Versions: a landed version is tagged v<version> at its first appearance on
# main's first-parent chain (config/extension.toml). A minor or major version
# (X.Y.0) also gets a GitHub Release with its CHANGELOG section as notes; a
# patch version is a tag only. Pushed tags are never moved.
#
#   bin/manure_mirror.sh [--dry-run] [--force]
#
# A run whose source head equals the last mirrored one exits at once unless
# --force; one run at a time holds $MIRROR/mirror.lock.
set -euo pipefail

DRY=0
FORCE=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY=1 ;;
        --force) FORCE=1 ;;
        *) echo "usage: $0 [--dry-run] [--force]" >&2; exit 2 ;;
    esac
done

H="$(cd "$(dirname "$0")/.." && pwd)"
SRC_REPO="${MANURE_SRC_REPO:-$HOME/tmp_workspace/isaac_sim}"
MIRROR="${MANURE_MIRROR_DIR:-$HOME/tmp_workspace/ai-worktrees/manure-mirror}"
PREFIX="extensions/env/worv.env.manure"
SRC_REF="refs/remotes/origin/ai/manure-mpm"
TARGET="MaumAI-Company/worv.env.manure"
TARGET_URL="https://github.com/${TARGET}.git"

if [ -f "$H/config/harness.env" ]; then
    export HARNESS_HOME="${HARNESS_HOME:-$H}"
    set +u -a
    # shellcheck disable=SC1091
    . "$H/config/harness.env"
    set -u +a
fi

log() { printf '%s manure_mirror: %s\n' "$(date -u +%FT%TZ)" "$*"; }

mkdir -p "$MIRROR"
exec 9>"$MIRROR/mirror.lock"
flock -n 9 || { log "another run holds the lock"; exit 0; }
git -C "$SRC_REPO" fetch -q origin ai/manure-mpm
source_now="$(git -C "$SRC_REPO" rev-parse "$SRC_REF")"
if [ "$FORCE" = 0 ] && [ "$DRY" = 0 ] && [ "$(cat "$MIRROR/last_source" 2>/dev/null)" = "$source_now" ]; then
    exit 0
fi
if [ ! -d "$MIRROR/src/.git" ]; then
    git init -q "$MIRROR/src"
fi
cd "$MIRROR/src"
git fetch -q --no-tags "$SRC_REPO" "+${SRC_REF}:refs/remotes/src/manure-mpm"
git sparse-checkout init --cone >/dev/null 2>&1 || true
git sparse-checkout set "$PREFIX" >/dev/null
git checkout -q --detach refs/remotes/src/manure-mpm
head_src="$(git rev-parse refs/remotes/src/manure-mpm)"

split="$(git subtree split --prefix="$PREFIX" refs/remotes/src/manure-mpm 2>/dev/null)"
git branch -f mirror-clean "$split"
FILTER_BRANCH_SQUELCH_WARNING=1 git filter-branch -f --msg-filter \
    "sed -E '/^[Cc]o-[Aa]uthored-[Bb]y:/d' | sed -e :a -e '/^\n*\$/{\$d;N;ba' -e '}'" \
    -- mirror-clean >/dev/null 2>&1

new_main="$(git rev-parse mirror-clean)"
if [ "$(git rev-parse "mirror-clean^{tree}")" != "$(git rev-parse "refs/remotes/src/manure-mpm:$PREFIX")" ]; then
    log "refused: the mirror's tree differs from $PREFIX at ${head_src:0:9}"
    exit 1
fi
if git log mirror-clean --format=%B | grep -qi '^co-authored-by:'; then
    log "refused: a Co-Authored-By trailer survived the filter"
    exit 1
fi

# A bare repo holding only the mirror's main and its tags, so nothing of
# isaac_sim's own refs or tags is ever pushed.
rm -rf "$MIRROR/push.git"
git init -q --bare "$MIRROR/push.git"
git -C "$MIRROR/push.git" fetch -q --no-tags "$MIRROR/src" "mirror-clean:refs/heads/main"
git -C "$MIRROR/push.git" fetch -q "$TARGET_URL" "+refs/heads/main:refs/remotes/target/main" \
    "+refs/tags/*:refs/tags/*" 2>/dev/null || true

remote_main="$(git -C "$MIRROR/push.git" rev-parse -q --verify refs/remotes/target/main || true)"
if [ -n "$remote_main" ] && ! git -C "$MIRROR/push.git" merge-base --is-ancestor "$remote_main" main; then
    log "refused: ${new_main:0:9} is not a fast-forward of the remote main ${remote_main:0:9}"
    exit 1
fi

# Versions on the first-parent chain, at their first appearance; skip tags
# that already exist.
mapfile -t new_tags < <(python3 -I - "$MIRROR/push.git" <<'PY'
import re, subprocess, sys
repo = sys.argv[1]
def run(*a, inp=None):
    return subprocess.run(["git", "-C", repo, *a], input=inp, capture_output=True,
                          text=True, check=True).stdout
fp = run("rev-list", "--first-parent", "--reverse", "main").split()
data = run("cat-file", "--batch", inp="".join(f"{c}:config/extension.toml\n" for c in fp))
existing = set(run("tag", "-l").split())
pos, prev, seen = 0, None, set()
for c in fp:
    end = data.index("\n", pos)
    header = data[pos:end]
    pos = end + 1
    version = None
    if not header.endswith("missing"):
        size = int(header.split()[2])
        body = data[pos:pos + size]
        pos += size + 1
        m = re.search(r'^version\s*=\s*"([^"]+)"', body, re.M)
        version = m.group(1) if m else None
    if version and version != prev and version not in seen:
        seen.add(version)
        if f"v{version}" not in existing:
            print(f"v{version} {c}")
    prev = version or prev
PY
)

log "source ${head_src:0:9}, mirror main ${new_main:0:9}, remote main ${remote_main:0:9}, new tags ${#new_tags[@]}"
if [ "$DRY" = 1 ]; then
    printf '%s\n' "${new_tags[@]}"
    exit 0
fi

if [ "$new_main" != "$remote_main" ]; then
    git -C "$MIRROR/push.git" push -q "$TARGET_URL" refs/heads/main:refs/heads/main
    log "pushed main ${new_main:0:9}"
fi
for line in "${new_tags[@]}"; do
    tag="${line%% *}"
    commit="${line##* }"
    git -C "$MIRROR/push.git" tag "$tag" "$commit"
    git -C "$MIRROR/push.git" push -q "$TARGET_URL" "refs/tags/$tag:refs/tags/$tag"
    log "tagged $tag at ${commit:0:9}"
    version="${tag#v}"
    if [[ "$version" =~ ^[0-9]+\.[0-9]+\.0$ ]]; then
        notes="$(mktemp)"
        git -C "$MIRROR/push.git" show "$tag:docs/CHANGELOG.md" |
            awk -v ver="$version" 'BEGIN{p=0} /^## \[/{ if (p) exit; if (index($0, "## [" ver "]")==1) {p=1; next} } p' >"$notes"
        gh release create "$tag" -R "$TARGET" --title "worv.env.manure $version" \
            --notes-file "$notes" --verify-tag </dev/null >/dev/null
        rm -f "$notes"
        log "released $tag"
    fi
done
printf '%s\n' "$head_src" >"$MIRROR/last_source"
