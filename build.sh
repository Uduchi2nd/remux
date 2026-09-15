#!/bin/bash
# Build the local compatibility branch; optionally rebase its patch stack.
# Usage: ~/remux-build/build.sh [upstream-tag]
set -euo pipefail
cd "$HOME/remux-build"
branch=kkphim-server-compat
test "$(git branch --show-current)" = "$branch" || {
 echo "Switch to $branch before building." >&2; exit 1;
}
test -z "$(git status --porcelain --untracked-files=no)" || {
 echo 'Commit or save tracked changes before building.' >&2; exit 1;
}
if [ -n "${1:-}" ]; then
 git fetch upstream --tags
 target=$(git rev-parse --verify "$1^{commit}")
 base=$(git config --get branch.kkphim-server-compat.upstreamBase || printf 'v0.31.0')
 git merge-base --is-ancestor "$base" HEAD
 git branch "backup/kkphim-$(date -u +%Y%m%dT%H%M%SZ)"
 git rebase --onto "$target" "$base" || {
  echo 'Rebase needs review. Resolve or abort it; do not deploy an older binary.' >&2
  exit 1
 }
 git config branch.kkphim-server-compat.upstreamBase "$target"
fi
git log --oneline -3
podman run --rm --name remux-release-build \
 -v "$HOME/remux-build:/work:Z" \
 -v "$HOME/remux-cargo-cache:/usr/local/cargo/registry:Z" -w /work \
 docker.io/library/rust:1-trixie cargo build --release -j 2 -p remux-server
sha256sum target/release/remux-server
