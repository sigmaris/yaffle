#!/bin/sh -eu
PROJECTDIR="$(pwd)"
cd ../Toshi
echo "Build Toshi in $(pwd)"
cargo build --release
cd "$PROJECTDIR"
echo "Build yaffle in $(pwd)"
cargo build --release
