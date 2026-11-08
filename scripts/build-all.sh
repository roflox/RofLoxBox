#!/bin/sh
# Runs build-macos.sh, build-linux.sh and build-windows.sh in turn. A failed
# build doesn't stop the others; the summary at the end says which failed,
# and the exit status is non-zero if any did. Arguments go to build-macos.sh
# (e.g. --universal). Everything lands in target/dist.
set -u
cd "$(dirname "$0")"
failed=""
for platform in macos linux windows; do
    echo "==> Building for $platform"
    if [ "$platform" = macos ]; then
        sh ./build-macos.sh "$@" || failed="$failed $platform"
    else
        sh "./build-$platform.sh" || failed="$failed $platform"
    fi
done
if [ -n "$failed" ]; then
    echo "==> Failed:$failed" >&2
    exit 1
fi
echo "==> All builds done: target/dist"
