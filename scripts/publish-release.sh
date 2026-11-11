#!/bin/sh
# Uploads the builds in target/dist/ to GitHub as a release, downloadable
# from the repository's Releases page, with their SHA-256 sums.
#
#   scripts/publish-release.sh           the pushed commit's version,
#                                        v0.<number of commits>.0
#   scripts/publish-release.sh --dev     the -dev builds (uncommitted changes
#                                        included), as a pre-release:
#                                        v0.<commits + 1>.0-dev
#   --draft (with either)                only you see it until you publish it
#
# Build first (scripts/build-macos.sh, build-windows.sh, build-linux.sh),
# and push the commit: the release points at it. Running it again for the
# same version replaces the files. Needs the GitHub CLI, once:
# `brew install gh && gh auth login`.
set -eu
cd "$(dirname "$0")/.."
command -v gh >/dev/null || { echo "Needs the GitHub CLI: brew install gh && gh auth login"; exit 1; }

DEV="" DRAFT=""
for arg in "$@"; do
    case $arg in
        --dev) DEV=1 ;;
        --draft) DRAFT=--draft ;;
        *) echo "Unknown option $arg (use --dev, --draft)"; exit 1 ;;
    esac
done

if [ -n "$DEV" ]; then
    # What the build scripts call this working copy (…-dev with changes).
    VERSION=$(scripts/version.sh)
    case $VERSION in
        *-dev) ;;
        *) echo "No uncommitted changes: this is $VERSION, publish it without --dev"; exit 1 ;;
    esac
    KIND=--prerelease
    TITLE="RofLoxBox $VERSION (pre-release)"
else
    # The commit's own version, whatever is uncommitted.
    VERSION=0.$(git rev-list --count HEAD).0
    KIND=""
    TITLE="RofLoxBox $VERSION"
    if ! git diff --quiet HEAD; then
        echo "Note: uncommitted changes are left out: publishing $VERSION, the commit's builds"
    fi
fi
TAG=v$VERSION
COMMIT=$(git rev-parse HEAD)
DIST=target/dist

# Only this version's builds (older ones may still be lying around).
# (RofLoxBox-0.25.0-* would also take RofLoxBox-0.25.0-dev-*.)
FILES=$(find "$DIST" -maxdepth 1 -type f -name "RofLoxBox-$VERSION-*" \
    ! -name "RofLoxBox-$VERSION-dev-*" | sort)
if [ -z "$FILES" ]; then
    echo "No builds of $VERSION in $DIST: run the build scripts first"
    exit 1
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
    # shellcheck disable=SC2086
    gh release create "$TAG" $FILES "$SUMS" $DRAFT $KIND --target "$COMMIT" \
        --title "$TITLE" --generate-notes
fi
gh release view "$TAG" --json url --jq .url
