#!/usr/bin/env bash
# Cut a release. Bumps Cargo.toml, commits, tags and pushes — the GitHub Actions
# release workflow does the actual publishing. Nothing is uploaded from here.
#
#   ./scripts/release.sh 0.2.1
#   ./scripts/release.sh 0.2.1 --dry-run
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="${1:-}"
DRY=""
[[ "${2:-}" == "--dry-run" ]] && DRY=1

say() { printf '\033[36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
run() { if [[ -n "$DRY" ]]; then printf '  \033[90m[dry-run] %s\033[0m\n' "$*"; else "$@"; fi; }

[[ -n "$VERSION" ]] || die "usage: $0 <version> [--dry-run]   e.g. $0 0.2.1"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be x.y.z, got '$VERSION'"

command -v gh >/dev/null || die "the GitHub CLI (gh) is required"
[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty — commit or stash first"

CURRENT=$(grep -m1 '^version *= *' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
say "current: $CURRENT  →  new: $VERSION"
[[ "$CURRENT" != "$VERSION" ]] || die "Cargo.toml is already at $VERSION"

if git rev-parse "v$VERSION" >/dev/null 2>&1; then
  die "tag v$VERSION already exists"
fi

# crates.io versions are immutable — refuse to re-cut one that is already live
if curl -sS -H 'User-Agent: networkcop-release' \
     https://crates.io/api/v1/crates/networkcop 2>/dev/null \
   | grep -q "\"num\":\"$VERSION\""; then
  die "networkcop $VERSION is already published to crates.io"
fi

say "bumping Cargo.toml"
run sed -i "s/^version = \"$CURRENT\"/version = \"$VERSION\"/" Cargo.toml

say "running the same gates CI will run"
if [[ -z "$DRY" ]]; then
  cargo fmt --all -- --check
  cargo clippy --all-targets -- -D warnings
  cargo test --all-targets
  RUSTFLAGS="-D warnings" cargo build --release
  cargo publish --dry-run
else
  printf '  \033[90m[dry-run] fmt · clippy · test · build · publish --dry-run\033[0m\n'
fi

say "committing and tagging"
run git add Cargo.toml Cargo.lock
run git commit -m "chore(release): v$VERSION"
run git tag -a "v$VERSION" -m "networkcop $VERSION"

say "pushing — this triggers the release workflow"
run git push origin main
run git push origin "v$VERSION"

say "done"
cat <<EOF

The release workflow is now running:
  gh run watch --exit-status

Once it finishes, anyone can update with:
  cargo install networkcop --force
EOF
