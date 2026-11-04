#!/bin/sh
# Uploads the builds in target/dist/ to GitHub as the release v<version>
# (0.<number of commits>.0, scripts/version.sh), downloadable from the
# repository's Releases page, with their SHA-256 sums.
#
#   scripts/publish-release.sh           publish
#   scripts/publish-release.sh --draft   only you see it until you publish it
#
# Build first (scripts/build-macos.sh, build-windows.sh, build-linux.sh),
# and push the commit: the release points at it. Running it again for the
# same version replaces the files. Needs the GitHub CLI, once:
# `brew install gh && gh auth login`.
set -eu
cd "$(dirname "$0")/.."
command -v gh >/dev/null || { echo "Needs the GitHub CLI: brew install gh && gh auth login"; exit 1; }
VERSION=$(scripts/version.sh)
TAG=v$VERSION
COMMIT=$(git rev-parse HEAD)
DIST=target/dist

# Only this version's builds (older ones may still be lying around).
FILES=$(find "$DIST" -maxdepth 1 -type f -name "RofLoxBox-$VERSION-*" | sort)
if [ -z "$FILES" ]; then
    echo "No builds of $VERSION in $DIST: run the build scripts first"
    exit 1
fi
if ! git diff --quiet HEAD -- crates; then
    echo "Note: crates/ has uncommitted changes; the builds may not match $TAG"
fi
if ! gh api "repos/{owner}/{repo}/commits/$COMMIT" --silent 2>/dev/null; then
    echo "Commit $COMMIT is not on GitHub yet: push it first"
    exit 1
fi

SUMS=$DIST/SHA256SUMS-$VERSION.txt
# shellcheck disable=SC2086
(cd "$DIST" && shasum -a 256 $(for f in $FILES; do basename "$f"; done)) > "$SUMS"
echo "Uploading to $TAG:"
for f in $FILES "$SUMS"; do echo "  $(basename "$f")"; done

if gh release view "$TAG" >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    gh release upload "$TAG" $FILES "$SUMS" --clobber
else
    DRAFT=""
    if [ "${1:-}" = "--draft" ]; then DRAFT=--draft; fi
    # shellcheck disable=SC2086
    gh release create "$TAG" $FILES "$SUMS" $DRAFT --target "$COMMIT" \
        --title "RofLoxBox $VERSION" --generate-notes
fi
gh release view "$TAG" --json url --jq .url
