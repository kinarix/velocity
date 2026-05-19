#!/usr/bin/env bash
#
# Velocity release helper — adapted from kinarix/callstack's
# `set-release-version.js`. Wired into the Makefile as `make release`.
#
# What it does
# ────────────
#   1. Reads current versions from workspace Cargo.toml + charts/velocity/Chart.yaml.
#   2. If only docs/website/runbooks paths have changed, treats this as a docs
#      push: skips the version bump, commits, pushes, exits.
#   3. Otherwise prompts for a new SemVer and what to release:
#        - cli    → tag `v<X>`         (release.yml builds binaries)
#        - chart  → tag `chart-v<X>`   (helm-publish.yml publishes the chart)
#        - both   → both tags          (default)
#   4. Updates Cargo.toml workspace.package.version, runs `cargo update --workspace`,
#      updates charts/velocity/Chart.yaml (version + appVersion).
#   5. Commits with an auto-generated message (overridable), pushes main,
#      creates the chosen tag(s) and pushes them. The tag push is what
#      actually triggers the release workflows; the main push is just so
#      reviewers can see what shipped.
#
# Safety
# ──────
#   - Dirty working tree is OK — the script shows what's about to be
#     swept up and prompts for a commit message. The dirty changes
#     and the version bump land as a single commit with that message.
#     Set VELOCITY_RELEASE_MSG to skip the prompt non-interactively.
#   - Refuses if the target tag already exists locally or remotely. The
#     release workflows are tag-triggered; a duplicate tag would either
#     no-op or, worse, race the prior run's artefacts.
#   - Refuses if you are not on `main`. Release commits go to the deploy
#     branch; tagging from a feature branch leaves a misleading history.
#
# Override knobs (env vars)
# ─────────────────────────
#   VELOCITY_RELEASE_KIND   cli|chart|both           (skip the prompt)
#   VELOCITY_RELEASE_VER    e.g. 0.2.0               (skip the prompt)
#   VELOCITY_RELEASE_MSG    custom commit message    (skip the prompt)
#   VELOCITY_BRANCH         main                     (allow tagging from another branch)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

CARGO_FILE="$REPO_ROOT/Cargo.toml"
CHART_FILE="$REPO_ROOT/charts/velocity/Chart.yaml"

# ANSI — keep narrow so the script is readable on a plain terminal.
B=$'\033[0;34m'; G=$'\033[0;32m'; Y=$'\033[1;33m'; R=$'\033[0;31m'; N=$'\033[0m'

die()   { echo "${R}error:${N} $*" >&2; exit 1; }
info()  { echo "${B}→${N} $*"; }
ok()    { echo "${G}✓${N} $*"; }
warn()  { echo "${Y}!${N} $*"; }

# ─── prerequisites ───────────────────────────────────────────────────────────
command -v git    >/dev/null || die "git not found"
command -v cargo  >/dev/null || die "cargo not found"
command -v sed    >/dev/null || die "sed not found"

[ -f "$CARGO_FILE" ] || die "$CARGO_FILE missing"
[ -f "$CHART_FILE" ] || die "$CHART_FILE missing"

# ─── git sanity ──────────────────────────────────────────────────────────────
branch="$(git rev-parse --abbrev-ref HEAD)"
expected_branch="${VELOCITY_BRANCH:-main}"

if [ "$branch" != "$expected_branch" ]; then
  die "on branch '$branch'; expected '$expected_branch' (override with VELOCITY_BRANCH)"
fi

# Working tree may be dirty — that's fine. We surface what would be
# swept up and capture a commit message now, then reuse it for the
# combined "your changes + version bump" commit so git history shows
# a single, intentional release commit instead of two.
release_msg="${VELOCITY_RELEASE_MSG:-}"

if ! git diff-index --quiet HEAD -- || [ -n "$(git ls-files --others --exclude-standard)" ]; then
  warn "working tree is dirty — these changes will be included in the release commit:"
  git status --short
  echo
  if [ -z "$release_msg" ]; then
    read -r -p "${B}commit message for the dirty changes (will also serve as the release commit)${N}: " release_msg
    if [ -z "$release_msg" ]; then
      die "a commit message is required when the working tree is dirty"
    fi
  fi
fi

# Fetch tags so the remote-existence check below sees current state.
info "git fetch --tags origin"
git fetch --tags origin >/dev/null 2>&1 || warn "could not fetch from origin — proceeding with local view"

# ─── docs-only escape hatch ──────────────────────────────────────────────────
# Mirror callstack: if every changed path is doc/website/runbook, this is a
# "redeploy the website" push, not a release. Don't bump versions, don't tag.
changed=$(
  {
    git diff --name-only HEAD
    git diff --name-only
    git ls-files --others --exclude-standard
  } | sed '/^$/d' | sort -u
)
non_doc=$(printf '%s\n' "$changed" | awk '
  $0 ~ /^docs\// {next}
  $0 ~ /^website\// {next}
  $0 ~ /^runbooks\// {next}
  $0 ~ /^README/ {next}
  $0 ~ /^CHANGELOG/ {next}
  {print}
')

if [ -n "$changed" ] && [ -z "$non_doc" ]; then
  warn "only docs/website/runbooks changed — skipping version bump"
  # If we already captured a message from the dirty-tree check, reuse
  # it. Otherwise prompt now with a docs-flavoured default.
  if [ -z "$release_msg" ]; then
    read -r -p "${B}commit message [docs: update]${N}: " release_msg
    release_msg="${release_msg:-docs: update}"
  fi
  git add docs website runbooks README* CHANGELOG* 2>/dev/null || true
  git commit -m "$release_msg"
  ok "committed: $release_msg"
  git push origin "$branch"
  ok "pushed origin/$branch (no release tags created)"
  exit 0
fi

# ─── version discovery ───────────────────────────────────────────────────────
# Workspace version lives in [workspace.package]; member crates inherit via
# `version.workspace = true`. So one sed against the root file covers all
# of the binary surface.
cur_cli="$(awk '
  /^\[workspace.package\]/ {in_block=1; next}
  /^\[/                   {in_block=0}
  in_block && /^version[[:space:]]*=/ {
    match($0, /"[^"]+"/); print substr($0, RSTART+1, RLENGTH-2); exit
  }
' "$CARGO_FILE")"

cur_chart="$(awk '/^version:/ {print $2; exit}' "$CHART_FILE")"
cur_app="$(  awk '/^appVersion:/ {print $2; exit}' "$CHART_FILE" | tr -d '"')"

echo
info "current versions"
echo "  cli (workspace):        ${Y}$cur_cli${N}"
echo "  chart/version:          ${Y}$cur_chart${N}"
echo "  chart/appVersion:       ${Y}$cur_app${N}"
echo

# ─── prompt: kind ────────────────────────────────────────────────────────────
kind="${VELOCITY_RELEASE_KIND:-}"
if [ -z "$kind" ]; then
  read -r -p "${B}release kind — cli|chart|both [both]${N}: " kind
  kind="${kind:-both}"
fi
case "$kind" in
  cli|chart|both) ;;
  *) die "invalid kind '$kind' — expected cli|chart|both" ;;
esac

# ─── prompt: version ─────────────────────────────────────────────────────────
ver="${VELOCITY_RELEASE_VER:-}"
if [ -z "$ver" ]; then
  default_ver="$cur_cli"
  case "$kind" in chart) default_ver="$cur_chart" ;; esac
  read -r -p "${B}new version (X.Y.Z) [bump from $default_ver]${N}: " ver
  ver="${ver:-}"
fi

[[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
  || die "version '$ver' is not SemVer (X.Y.Z or X.Y.Z-prerelease)"

# ─── tag pre-flight ──────────────────────────────────────────────────────────
# Pre-flight every tag we intend to create before we start mutating files.
# Catching "tag exists" after we've already bumped Cargo.toml leaves the
# tree in a half-released state which is annoying to back out.
tags_to_create=()
case "$kind" in
  cli)   tags_to_create=("v${ver}") ;;
  chart) tags_to_create=("chart-v${ver}") ;;
  both)  tags_to_create=("v${ver}" "chart-v${ver}") ;;
esac

for t in "${tags_to_create[@]}"; do
  if git rev-parse "$t" >/dev/null 2>&1; then
    die "tag '$t' already exists locally"
  fi
  if git ls-remote --tags origin "refs/tags/$t" | grep -q .; then
    die "tag '$t' already exists on origin"
  fi
done

# ─── apply version bumps ─────────────────────────────────────────────────────
echo
info "updating files..."

if [ "$kind" = "cli" ] || [ "$kind" = "both" ]; then
  # Replace ONLY the version inside [workspace.package] — don't touch member
  # crate sections (they should all be `version.workspace = true` but we
  # don't want to clobber any deliberate exceptions).
  awk -v new="$ver" '
    /^\[workspace.package\]/ {in_block=1; print; next}
    /^\[/                   {in_block=0; print; next}
    in_block && /^version[[:space:]]*=/ {
      print "version      = \"" new "\""
      next
    }
    {print}
  ' "$CARGO_FILE" > "$CARGO_FILE.tmp" && mv "$CARGO_FILE.tmp" "$CARGO_FILE"
  ok "Cargo.toml workspace.package.version → $ver"

  # `cargo update -w` (workspace) refreshes only velocity-* package versions
  # in Cargo.lock; it won't pull in unrelated crate updates that would
  # bloat the diff.
  info "cargo update --workspace"
  cargo update --workspace >/dev/null
  ok "Cargo.lock refreshed"
fi

if [ "$kind" = "chart" ] || [ "$kind" = "both" ]; then
  # `sed -i` flags differ between GNU and BSD sed — write through a temp
  # file to avoid the portability dance.
  awk -v new="$ver" '
    /^version:/    {print "version: " new; next}
    /^appVersion:/ {print "appVersion: \"" new "\""; next}
    {print}
  ' "$CHART_FILE" > "$CHART_FILE.tmp" && mv "$CHART_FILE.tmp" "$CHART_FILE"
  ok "Chart.yaml version + appVersion → $ver"
fi

# ─── commit + push main ──────────────────────────────────────────────────────
echo
default_msg="release: ${kind} v${ver}"
# Precedence: VELOCITY_RELEASE_MSG > the message captured for dirty
# changes earlier > prompt with the auto-generated default.
msg="${VELOCITY_RELEASE_MSG:-${release_msg:-}}"
if [ -z "$msg" ]; then
  read -r -p "${B}commit message [${default_msg}]${N}: " msg
  msg="${msg:-$default_msg}"
fi

git add -A
git commit -m "$msg"
ok "committed: $msg"

git push origin "$branch"
ok "pushed origin/$branch"

# ─── tag + push tags ─────────────────────────────────────────────────────────
echo
info "creating tags: ${tags_to_create[*]}"
for t in "${tags_to_create[@]}"; do
  git tag -a "$t" -m "$msg"
  ok "tag $t"
done

# One push for all the tags so the two release workflows kick off close
# enough together to avoid weirdness with a half-released state.
git push origin "${tags_to_create[@]}"
ok "pushed tag(s) — release workflows will start"

echo
ok "release flow complete"
echo "  Actions:  https://github.com/$(git config --get remote.origin.url | sed -E 's|.*github\.com[:/]([^/]+/[^/.]+)(\.git)?$|\1|')/actions"
case "$kind" in
  cli)   echo "  Watch:    release.yml" ;;
  chart) echo "  Watch:    helm-publish.yml" ;;
  both)  echo "  Watch:    release.yml + helm-publish.yml" ;;
esac
