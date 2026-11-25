#!/bin/bash
# Set the Claude Code plugin's version from what it carries.
#
#   ./scripts/plugin-version.sh           write it into plugin.json
#   ./scripts/plugin-version.sh --check   fail unless plugin.json already has it
#
# The version is a hash of the plugin's tracked files (all but plugin.json), so it
# only moves when the skill, the /start command or the bin/ scripts change: a
# release that leaves them alone leaves the plugin's version alone, and Claude Code
# has nothing new to pick up. scripts/release.sh writes it before it tags, and the
# release workflow runs --check against the tag (plugin/README.md, "The version").
#
# It's semver with the hash as a prerelease tag; the h keeps an all-digit hash
# from reading as a number.
set -euo pipefail
cd "$(dirname "$0")/../plugin/task-board"

manifest=.claude-plugin/plugin.json

# shasum ships with macOS and most Linux; Git Bash on Windows may only have sha256sum.
if command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256; }
else
  sha256() { sha256sum; }
fi

# Hash each file as git stores it (its staged blob), not the working copy: a Windows
# checkout turns LF into CRLF and would hash to another version. Each line keeps
# shasum's "<hash>  <path>" form, so the version matches hashing the files directly.
files() {
  local meta path blob
  while IFS= read -r -d '' meta; do
    path=${meta#*$'\t'}
    blob=${meta#* }
    blob=${blob%% *}
    echo "$(git cat-file blob "$blob" | sha256 | cut -d' ' -f1)  $path"
  done < <(git ls-files -s -z -- . ":!$manifest")
}

# The hash covers what's staged, so an unstaged edit wouldn't move it: stop rather
# than write a version the files on disk don't make.
if ! git diff --quiet -- . ":!$manifest"; then
  echo "The plugin has unstaged changes; the version is a hash of what's staged. Stage them first: git add plugin/task-board" >&2
  git diff --name-only -- . ":!$manifest" >&2
  exit 1
fi

hash=$(files | sha256 | cut -c1-12)
version="1.0.0-h$hash"
current=$(sed -n 's/^ *"version": *"\([^"]*\)".*/\1/p' "$manifest" | head -1)

if [ "${1:-}" = "--check" ]; then
  if [ "$current" != "$version" ]; then
    echo "plugin.json says $current; its files make $version. Run: ./scripts/plugin-version.sh" >&2
    exit 1
  fi
  echo "plugin $version"
  exit 0
fi

# Through a temp file, not sed -i: BSD and GNU sed read -i differently.
tmp=$(mktemp "$manifest.XXXXXX")
sed "s/^\( *\"version\": *\"\)[^\"]*\"/\1$version\"/" "$manifest" >"$tmp"
mv "$tmp" "$manifest"
echo "plugin $version"
