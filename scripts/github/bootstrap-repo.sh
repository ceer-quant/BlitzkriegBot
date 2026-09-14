#!/usr/bin/env bash
# Idempotent GitHub-side bootstrap for ceer-quant/BlitzkriegBot.
#
# This script performs the SERVER-SIDE steps of the repository-governance plan
# that cannot be done from the working tree: create the private repo, push,
# set branch protection, apply labels, enable Secret Scanning + Push Protection,
# and register the WeChat webhook secret.
#
# It is IDEMPOTENT and SAFE-BY-DEFAULT:
#   * default mode is --dry-run (prints what it WOULD do, changes nothing)
#   * every step is guarded so re-running is a no-op
#   * it REFUSES to push to the current upstream (alsk1992/CloddsBot)
#
# Usage:
#   bash scripts/github/bootstrap-repo.sh                 # dry-run, prints plan
#   bash scripts/github/bootstrap-repo.sh --execute       # actually apply
#   bash scripts/github/bootstrap-repo.sh --execute --labels-only
#   bash scripts/github/bootstrap-repo.sh --execute --wechat <webhook-url>
#
# Requirements: gh (GitHub CLI) authenticated with repo/admin scope.
set -uo pipefail

ORG="ceer-quant"
REPO="BlitzkriegBot"
FULL="${ORG}/${REPO}"
UPSTREAM_FORBIDDEN="alsk1992/CloddsBot"

EXECUTE=0
LABELS_ONLY=0
WECHAT_URL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --execute)     EXECUTE=1 ;;
    --labels-only) LABELS_ONLY=1 ;;
    --wechat)      shift; WECHAT_URL="${1:-}" ;;
    -h|--help)     sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

run() {
  if [ "$EXECUTE" -eq 1 ]; then
    echo "  \$ $*"
    "$@"
  else
    echo "  [dry-run] $*"
  fi
}
section() { echo; echo "== $* =="; }

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "not a git repo" >&2; exit 2; }
cd "$ROOT"

section "preflight"
if ! command -v gh >/dev/null 2>&1; then
  echo "ERROR: gh (GitHub CLI) not found. Install it, then 'gh auth login'." >&2
  echo "       On macOS: brew install gh" >&2
  exit 2
fi
if ! gh auth status >/dev/null 2>&1; then
  echo "ERROR: gh is not authenticated. Run 'gh auth login'." >&2
  exit 2
fi
echo "gh: $(gh --version | head -1)"

CUR_ORIGIN="$(git remote get-url origin 2>/dev/null || echo '(none)')"
echo "current origin: $CUR_ORIGIN"
case "$CUR_ORIGIN" in
  *"$UPSTREAM_FORBIDDEN"*)
    echo "WARNING: current origin is the forbidden upstream '$UPSTREAM_FORBIDDEN'." >&2
    echo "         This script will target a NEW remote 'ceer' -> $FULL and never touch origin." ;;
esac

if [ "$LABELS_ONLY" -eq 1 ]; then
  section "labels only"
else
  section "1. create private repo $FULL (no-op if it exists)"
  if gh repo view "$FULL" >/dev/null 2>&1; then
    echo "  repo already exists — skipping create"
  else
    run gh repo create "$FULL" --private --description "Private HFT trading core (Rust) + Node shell"
  fi

  section "2. add remote 'ceer' and push branches (never origin)"
  if git remote get-url ceer >/dev/null 2>&1; then
    echo "  remote 'ceer' already configured"
  else
    run git remote add ceer "git@github.com:${FULL}.git"
  fi
  # Push EVERY local branch (main, rust-core-p0, feature branches) so the new
  # repo receives the full history — never just a possibly-stale main.
  run git push ceer --all
  run git push ceer --tags
  # Seed develop from the branch that actually carries the work (rust-core-p0 is
  # 13 commits ahead of main at time of writing). Override with INTEGRATION_BRANCH.
  INTEGRATION_BRANCH="${INTEGRATION_BRANCH:-rust-core-p0}"
  echo "  seeding develop from '$INTEGRATION_BRANCH'"
  run gh api --method POST "repos/${FULL}/git/refs" \
    -f ref="refs/heads/develop" -f sha="$(git rev-parse "$INTEGRATION_BRANCH")" 2>/dev/null || true

  section "3. enable Secret Scanning + Push Protection"
  run gh api --method PATCH "repos/${FULL}" \
    -F "security_and_analysis[secret_scanning][status]=enabled" \
    -F "security_and_analysis[secret_scanning_push_protection][status]=enabled"
  # Also enable via the dedicated endpoints (older gh versions).
  run gh api --method PATCH "repos/${FULL}/secret-scanning/push-protection" 2>/dev/null || true

  section "4. branch protection: main / develop"
  for BR in main develop; do
    run gh api --method PUT "repos/${FULL}/branches/${BR}/protection" \
      -F "required_status_checks[strict]=true" \
      -F "required_status_checks[contexts][]=rust-check" \
      -F "required_status_checks[contexts][]=node-check" \
      -F "required_status_checks[contexts][]=secret-scan" \
      -F "enforce_admins=true" \
      -F "required_pull_request_reviews[required_approving_review_count]=1" \
      -F "restrictions=" \
      -F "allow_force_pushes=false" \
      -F "allow_deletions=false"
  done

  section "5. WeChat webhook secret"
  if [ -n "$WECHAT_URL" ]; then
    if [ "$EXECUTE" -eq 1 ]; then
      printf '%s' "$WECHAT_URL" | gh secret set WECHAT_WEBHOOK --repo "$FULL"
      echo "  set WECHAT_WEBHOOK"
    else
      echo "  [dry-run] gh secret set WECHAT_WEBHOOK --repo $FULL"
    fi
  else
    echo "  (no --wechat <url> given; CI notify job will silently skip)"
  fi
fi

section "6. apply labels from .github/labels.yml"
if [ ! -f .github/labels.yml ]; then
  echo "  missing .github/labels.yml — skipping"
else
  # Parse the minimal YAML: "- name:" / "color:" / "description:" triples.
  python3 - <<'PY' > /tmp/blitz_labels.tsv
import re, sys
rec = {}
for line in open('.github/labels.yml', encoding='utf-8'):
    s = line.strip()
    m = re.match(r'^-\s*name:\s*"?(.*?)"?$', s)
    if m:
        if rec.get('name'): print(f"{rec['name']}\t{rec.get('color','ededed')}\t{rec.get('description','')}")
        rec = {'name': m.group(1)}
        continue
    m = re.match(r'^color:\s*"?(.*?)"?$', s)
    if m: rec['color'] = m.group(1); continue
    m = re.match(r'^description:\s*"?(.*?)"?$', s)
    if m: rec['description'] = m.group(1); continue
if rec.get('name'): print(f"{rec['name']}\t{rec.get('color','ededed')}\t{rec.get('description','')}")
PY
  while IFS=$'\t' read -r name color desc; do
    [ -z "$name" ] && continue
    run gh label create "$name" --color "$color" --description "$desc" --force --repo "$FULL"
  done < /tmp/blitz_labels.tsv
  echo "  labels parsed: $(wc -l < /tmp/blitz_labels.tsv | tr -d ' ')"
fi

section "done"
if [ "$EXECUTE" -eq 0 ]; then
  echo "DRY-RUN complete. Re-run with --execute to apply."
else
  echo "Bootstrap complete for $FULL."
fi
