#!/usr/bin/env bash
# Create the bare repository that tests/git_http_server.py serves.
#
# Deliberately built with the real `git` binary, not with grit: the network
# tests compare grit's answers against what git does, and a fixture grit made
# would only prove grit agrees with itself.
set -euo pipefail

root="${1:?usage: setup_fixture.sh <dir>}"
mkdir -p "$root"

work="$root/work"
rm -rf "$work" "$root/upstream.git"
mkdir -p "$work"

cd "$work"
git init -q -b main .
git config user.name "Fixture"
git config user.email "fixture@example.com"
printf 'first\n' > a.md
mkdir -p docs
printf 'nested\n' > docs/b.md
git add -A
git commit -q -m "first"

printf 'second\n' >> a.md
git add -A
git commit -q -m "second"

git clone -q --bare . "$root/upstream.git"
git -C "$root/upstream.git" symbolic-ref HEAD refs/heads/main

echo "fixture ready: $root/upstream.git"
echo "serve it with:"
echo "  GIT_HTTP_ROOT=$root GIT_HTTP_USER=tester GIT_HTTP_PASS=s3cret \\"
echo "    python3 tests/git_http_server.py 8099"
