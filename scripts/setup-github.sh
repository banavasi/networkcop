#!/usr/bin/env bash
# Create the GitHub repository for networkcop and push this tree to it.
#
#   ./scripts/setup-github.sh                 # private (default)
#   ./scripts/setup-github.sh --public        # public
#   ./scripts/setup-github.sh --public --dry-run
#
# Idempotent: safe to re-run. If the repo or the remote already exists it says so
# and moves on rather than failing.
set -euo pipefail

REPO_NAME="${REPO_NAME:-networkcop}"
VISIBILITY="--private"
DRY_RUN=""
DESCRIPTION="Terminal agent harness for front-end debugging — captures a Chrome DevTools session and answers questions strictly from it."

while [[ $# -gt 0 ]]; do
  case "$1" in
    --public)  VISIBILITY="--public"; shift ;;
    --private) VISIBILITY="--private"; shift ;;
    --dry-run) DRY_RUN="1"; shift ;;
    --name)    REPO_NAME="$2"; shift 2 ;;
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
run() {
  if [[ -n "$DRY_RUN" ]]; then
    printf '  \033[90m[dry-run] %s\033[0m\n' "$*"
  else
    "$@"
  fi
}

cd "$(dirname "$0")/.."

command -v git >/dev/null || die "git is not installed"
command -v gh  >/dev/null || die "the GitHub CLI (gh) is not installed — see https://cli.github.com"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated — run: gh auth login"

OWNER="$(gh api user --jq .login)"
say "authenticated as ${OWNER}"

# --- sanity: the crate name and the repo name should agree ---
if [[ -f Cargo.toml ]]; then
  CRATE="$(grep -m1 '^name *=' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
  [[ "$CRATE" == "$REPO_NAME" ]] || say "note: crate is '${CRATE}' but repo will be '${REPO_NAME}'"
fi

# --- git init ---
if [[ ! -d .git ]]; then
  say "initialising git"
  run git init -b main
else
  say "git repository already present"
fi

# --- commit whatever is outstanding ---
if [[ -n "$(git status --porcelain)" ]]; then
  say "staging and committing working tree"
  run git add -A
  run git commit -m "feat: networkcop — CDP capture, TUI, and scope-guarded agent"
else
  say "working tree is clean"
fi

if ! git rev-parse HEAD >/dev/null 2>&1; then
  die "no commits — nothing to push"
fi

# --- create the remote repo ---
if gh repo view "${OWNER}/${REPO_NAME}" >/dev/null 2>&1; then
  say "repository ${OWNER}/${REPO_NAME} already exists"
else
  if [[ "$VISIBILITY" == "--public" ]]; then
    say "creating PUBLIC repository ${OWNER}/${REPO_NAME}"
    printf '    this makes the code visible to everyone. ctrl-c within 5s to abort.\n'
    [[ -n "$DRY_RUN" ]] || sleep 5
  else
    say "creating private repository ${OWNER}/${REPO_NAME}"
  fi
  run gh repo create "${REPO_NAME}" "${VISIBILITY}" --description "${DESCRIPTION}" --source=. --remote=origin
fi

# --- wire up the remote if the create step did not ---
if ! git remote get-url origin >/dev/null 2>&1; then
  say "adding origin"
  run git remote add origin "https://github.com/${OWNER}/${REPO_NAME}.git"
fi

say "pushing main"
run git push -u origin main

say "done → https://github.com/${OWNER}/${REPO_NAME}"

cat <<'EOF'

Next, to publish to crates.io:

  cargo login                 # paste a token from https://crates.io/settings/tokens
  cargo publish --dry-run     # verify the package
  cargo publish               # irreversible: the name and version are claimed forever

EOF
