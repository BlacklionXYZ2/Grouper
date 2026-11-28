#!/usr/bin/env bash
# Attach a CI build to a GitHub release. Called by the Desktop and Apple workflows.
#
#   scripts/publish-release.sh <file> <asset-name> <label>
#
# A tag push (v1.2.3) puts the file on that version's release.
# A push to a branch puts it on the rolling "<branch>-latest" pre-release, which
# always holds the newest successful build of that branch. Both workflows upload
# to the same release, so each one creates it if it isn't there yet.
#
# Needs GH_TOKEN with contents: write, and GITHUB_REF_TYPE/GITHUB_REF_NAME/GITHUB_SHA
# from Actions.
set -euo pipefail

file="$1"
asset="$2"
label="$3"
short="${GITHUB_SHA:0:7}"

if [ "$GITHUB_REF_TYPE" = "tag" ]; then
  tag="$GITHUB_REF_NAME"
  title="Grouper $tag"
  notes="Grouper $tag. See README.md, \"Download the apps\", for how to open the unsigned builds."
  flags=(--verify-tag)
else
  tag="${GITHUB_REF_NAME//\//-}-latest"
  title="Latest build ($GITHUB_REF_NAME)"
  notes="The newest successful build of the \`$GITHUB_REF_NAME\` branch, replaced on every push. Each file's label names the commit it was built from.

These builds are unsigned. See README.md, \"Download the apps\", for how to open them."
  flags=(--prerelease --latest=false --target "$GITHUB_SHA")
fi

# Create the release unless the other workflow already did.
if ! gh release view "$tag" >/dev/null 2>&1; then
  gh release create "$tag" --title "$title" --notes "$notes" "${flags[@]}" \
    || gh release view "$tag" >/dev/null
fi

if [ "$GITHUB_REF_TYPE" != "tag" ]; then
  # Move the rolling tag to this commit so "Source code" matches the newest build.
  gh api -X PATCH "repos/$GITHUB_REPOSITORY/git/refs/tags/$tag" \
    -f sha="$GITHUB_SHA" -F force=true >/dev/null
fi

staged="$(mktemp -d)/$asset"
cp "$file" "$staged"
gh release upload "$tag" "$staged#$label ($short)" --clobber
echo "Uploaded $asset to release $tag"
