#!/bin/sh
# RofLoxBox's version: 0.<number of commits>.0, so every commit is a new
# version. Outside a git checkout: the version in crates/app/Cargo.toml.
cd "$(dirname "$0")/.."
if count=$(git rev-list --count HEAD 2>/dev/null); then
    echo "0.$count.0"
else
    sed -n 's/^version = "\(.*\)"/\1/p' crates/app/Cargo.toml | head -1
fi
