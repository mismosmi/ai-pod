#!/bin/sh
set -e

VERSION="$1"
TAG_MESSAGE="$2"

if [ -z "$VERSION" ] || [ -z "$TAG_MESSAGE" ]; then
    echo "Usage: sh release.sh <version> <tag message>"
    echo "Example: sh release.sh 0.5.3 'increase timeout for command requests'"
    exit 1
fi

# Ensure we are on main
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
    echo "Error: not on main branch (current: $BRANCH)"
    exit 1
fi

# Ensure working directory is clean
if [ -n "$(git status --porcelain)" ]; then
    echo "Error: working directory is not clean"
    git status --short
    exit 1
fi

# Fetch and check up to date
git fetch origin
LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/main)
if [ "$LOCAL" != "$REMOTE" ]; then
    echo "Error: local main is not up to date with origin/main"
    exit 1
fi

# Update version in Cargo.toml
sed -i.bak "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml && rm Cargo.toml.bak

# Run tests
cargo test

# Commit version bump
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to $VERSION"

# Push
git push -f

# Create annotated tag and push
git tag -a "v$VERSION" -m "$TAG_MESSAGE"
git push --tags
