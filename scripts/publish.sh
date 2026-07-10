#!/usr/bin/env bash
#
# Publish dist/ artifacts to GitHub releases on this repository.
#
# Without --release: uploads unversioned artifacts to a rolling "latest"
# GitHub release.  No versioned release entity is created and no git tag
# is applied.  Useful for pushing an updated binary without bumping the version.
#
# With --release: creates (or recreates) a versioned GitHub release with
# both versioned and unversioned artifacts, and tags the current commit.
# If the release already exists on GitHub it is deleted and recreated, and
# the local git tag is force-updated.
#
# Prerequisites:
#   - gh (GitHub CLI): https://cli.github.com
#   - gh auth login
#   - dist/ populated by scripts/build.sh
#
# Usage:
#   ./scripts/publish.sh             # rolling latest
#   ./scripts/publish.sh --release   # versioned release + git tag

set -euo pipefail

DIST="${DIST:-dist}"
DO_RELEASE=false

for arg in "$@"; do
    case "$arg" in
        --release) DO_RELEASE=true ;;
        *) echo "error: unknown argument: $arg"; exit 1 ;;
    esac
done

# Derive owner/repo from the git remote so this works regardless of fork/mirror.
# Handles both SSH (git@github.com:owner/repo.git) and HTTPS forms.
remote_url="$(git remote get-url origin 2>/dev/null)" || {
    echo "error: could not read origin remote from git config"
    exit 1
}
REPO="$(echo "$remote_url" | sed -E 's|.*[:/]([^/]+/[^/]+)\.git$|\1|; s|.*[:/]([^/]+/[^/]+)$|\1|')"

VERSION="$(grep '^version' cli/Cargo.toml | head -1 | sed 's/.*= "\(.*\)"/\1/')"
VERSION_TAG="v$VERSION"

mapfile -t unversioned < <(find "$DIST" -maxdepth 1 -name "agent-browser-*" -not -name "agent-browser-$VERSION-*" -type f | sort)

if [[ ${#unversioned[@]} -eq 0 ]]; then
    echo "error: no artifacts found in $DIST/. Run scripts/build.sh first."
    exit 1
fi

if [[ "$DO_RELEASE" == true ]]; then
    TAG="$VERSION_TAG"
    versioned=()
    for f in "${unversioned[@]}"; do
        base="$(basename "$f")"
        # agent-browser-linux-x64 -> agent-browser-0.1.0-linux-x64
        dest="$DIST/agent-browser-$VERSION-${base#agent-browser-}"
        cp "$f" "$dest"
        versioned+=("$dest")
    done
    artifacts=("${versioned[@]}" "${unversioned[@]}")
else
    TAG="latest"
    artifacts=("${unversioned[@]}")
fi

echo "Tag:        $TAG"
echo "Repo:       $REPO"
echo "Artifacts:"
for f in "${artifacts[@]}"; do
    printf "  %s\n" "$(basename "$f")"
done
echo ""

if gh release view "$TAG" --repo "$REPO" &>/dev/null; then
    if [[ "$DO_RELEASE" == true ]]; then
        echo "$TAG already exists on $REPO — deleting and re-releasing."
    fi
    gh release delete "$TAG" --repo "$REPO" --cleanup-tag --yes
fi

if [[ "$DO_RELEASE" == true ]]; then
    NOTES_FILE="$(mktemp)"
    awk '/<!-- release:start -->/{found=1; next} /<!-- release:end -->/{found=0} found{print}' CHANGELOG.md > "$NOTES_FILE"
    gh release create "$TAG" \
        --repo "$REPO" \
        --title "$TAG" \
        --notes-file "$NOTES_FILE" \
        "${artifacts[@]}"
    rm -f "$NOTES_FILE"
else
    gh release create "$TAG" \
        --repo "$REPO" \
        --title "$TAG" \
        --notes '' \
        "${artifacts[@]}"
fi

if [[ "$DO_RELEASE" == true ]]; then
    if git rev-parse "$TAG" &>/dev/null 2>&1; then
        git tag -f "$TAG"
        echo "Re-tagged local commit $(git rev-parse --short HEAD) as $TAG"
    else
        git tag "$TAG"
        echo "Tagged local commit $(git rev-parse --short HEAD) as $TAG"
    fi
fi

echo "Published: https://github.com/$REPO/releases/tag/$TAG"
