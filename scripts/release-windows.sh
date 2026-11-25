#!/bin/bash
# Cut a Windows release: check the version, tag it and push the tag.
#
#   tb release cut windows-v0.1.0-beta.97.w.1         a build of a beta base (a prerelease)
#   tb release cut windows-v0.2.0-w.1                 a build of a stable base
#   scripts/release-windows.sh <version> --dry-run    every check, then stop before the tag
#
# Windows releases ship from the fork (slider9/taskboard) and only from feat/windows-native, betas
# and full releases alike. tb release cut runs this script with TB_RELEASE set and records the
# release on the board when it succeeds. Run on its own, the script refuses unless you pass
# --force or --dry-run. It takes windows-v<version>, v<version> or <version>.
#
# A Windows version is the upstream base the files say (Cargo.toml) plus a counter that lives
# only in the tag: <base>.w.N on a beta base, <base>-w.N on a stable one (docs/RELEASING.md,
# "Windows releases"). So unlike release.sh this never writes a version or commits; it only
# tags the branch's head. It looks only at windows-v tags: bare v tags are macOS's.
#
# A release whose workflow failed before it published is marked failed on the board (tb release
# fail R<n>); cutting the same name again sets TB_RELEASE_REPLACE, and this script then moves
# the tag to HEAD and force-pushes it, which runs the workflow again. A published version is
# never cut again.
#
# Pushing the tag runs .github/workflows/release-windows.yml, which builds the installer and the
# update zip and publishes the GitHub release.
set -euo pipefail
cd "$(dirname "$0")/.."

branch_from=feat/windows-native

force=
dry_run=
args=()
for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    --dry-run) dry_run=1 ;;
    *) args+=("$arg") ;;
  esac
done
version="${args[0]:-}"
version="${version#windows-}"
version="${version#v}"
if [ -z "${TB_RELEASE:-}" ] && [ -z "$force" ] && [ -z "$dry_run" ]; then
  echo "Run tb release cut windows-v${version:-<version>} instead; it runs this script and records the release. (--force to run it without the board, --dry-run to only check.)" >&2
  exit 1
fi
if [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+-beta\.[0-9]+)\.w\.([0-9]+)$ ]]; then
  :
elif [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-w\.([0-9]+)$ ]]; then
  :
else
  echo "Usage: $0 [windows-v|v]<base>.w.N (beta base, 0.1.0-beta.97.w.1) or <base>-w.N (stable base, 0.2.0-w.1) [--dry-run] [--force]" >&2
  exit 1
fi
base="${BASH_REMATCH[1]}"
counter="${BASH_REMATCH[2]}"
tag="windows-v$version"
if [[ "$counter" == 0* ]]; then
  echo "$tag: the counter starts at 1 and has no leading zeros." >&2
  exit 1
fi

# The fork's slug, for gh: with an upstream remote beside origin, gh would pick upstream's.
repo=${TASKBOARD_RELEASE_REPO:-$(git remote get-url origin | sed -E 's#^(https://github\.com/|git@github\.com:)##; s#\.git$##')}
if ! [[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "origin isn't a GitHub repo ($(git remote get-url origin)); set TASKBOARD_RELEASE_REPO=<owner>/<repo>." >&2
  exit 1
fi

# The board runs this in the background: a push that wants a password or a sign-in window must
# fail instead of waiting for one.
export GIT_TERMINAL_PROMPT=0 GCM_INTERACTIVE=never

branch=$(git branch --show-current)
if [ "$branch" != "$branch_from" ]; then
  echo "Windows releases go out from $branch_from (on ${branch:-a detached HEAD})." >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "Commit or stash your changes first." >&2
  exit 1
fi
git fetch -q origin "$branch_from"
if [ "$(git rev-parse HEAD)" != "$(git rev-parse "origin/$branch_from")" ]; then
  echo "$branch_from isn't the same as origin's: pull (or push) it first, so the tag is on the commit everyone has." >&2
  git status -sb | head -1 >&2
  exit 1
fi

# The files' upstream base (they all have to agree; set-version.sh --check says which).
if ! files=$(./scripts/set-version.sh --check 2>&1); then
  echo "$files" >&2
  exit 1
fi
files_base=$(echo "$files" | awk 'NR==1{print $2}')
# The plugin's version is a hash of its files, and Claude Code only reloads a plugin whose version
# moved. This script never commits, so a change to the plugin brings its new version in its PR.
if ! out=$(./scripts/plugin-version.sh --check 2>&1); then
  echo "$out" >&2
  echo "The plugin's version doesn't match its files: run scripts/plugin-version.sh in a PR into $branch_from, merge it, then cut again." >&2
  exit 1
fi

# A sortable key for a Windows version, ordered like semver (which the updater uses):
# 0.1.0-beta.97.w.1 < 0.1.0-beta.97.w.2 < 0.1.0-beta.98.w.1 < 0.1.0-w.1 < 0.2.0-beta.1.w.1.
# scripts/newest-version.py only knows upstream's versions.
vkey() {
  local v="$1" core rest beta=0 stable=1 n
  core="${v%%-*}"
  rest="${v#*-}"
  n="${v##*w.}"
  if [[ "$rest" == beta.* ]]; then
    stable=0
    beta="${rest#beta.}"
    beta="${beta%%.*}"
  fi
  IFS=. read -r a b c <<<"$core"
  printf '%09d.%09d.%09d.%d.%09d.%09d\n' "$((10#$a))" "$((10#$b))" "$((10#$c))" "$stable" "$((10#$beta))" "$((10#$n))"
}
newest() {
  local v
  for v in "$@"; do echo "$(vkey "$v") $v"; done | sort | tail -1 | cut -d' ' -f2
}

# Every Windows version tagged here or on origin. A tag only on this checkout still counts: it
# may be one whose push failed, or one deleted from origin that this checkout should drop too.
remote_tags=$(git ls-remote --tags --refs origin 'refs/tags/windows-v*') || {
  echo "Couldn't list origin's tags." >&2
  exit 1
}
versions=$({ git tag --list 'windows-v*'; echo "$remote_tags" | sed 's#.*refs/tags/##'; } |
  sed 's/^windows-v//' | sort -u | grep -E '^[0-9]+\.[0-9]+\.[0-9]+(-beta\.[0-9]+\.w\.[0-9]+|-w\.[0-9]+)$' || true)

# The counter after the newest build of a base, or 1 for a base with none.
next_of() {
  local b="$1" s n
  [[ "$b" == *-beta.* ]] && s=. || s=-
  n=$(echo "$versions" | grep -E "^${b//./\\.}${s/./\\.}w\.[0-9]+$" | sed -E 's/.*w\.//' | sort -n | tail -1 || true)
  echo "$b${s}w.$(( ${n:-0} + 1 ))"
}

# A tag's release: published, draft (a workflow_dispatch run, or a publish that failed halfway)
# or none. Any other gh failure (network, sign-in) stops the cut: guessing "none" would say to
# replace a published tag. gh says "release not found" for a missing repo too, so the repo is
# checked once up front.
if ! out=$(gh repo view "$repo" --json name -q .name 2>&1); then
  echo "Couldn't reach $repo with gh: $out" >&2
  exit 1
fi
release_state() {
  local out
  if out=$(gh release view "$1" --repo "$repo" --json isDraft -q .isDraft 2>&1); then
    case "$out" in
      false) echo published ;;
      true) echo draft ;;
      *) echo "gh said '$out' for $1's release on $repo." >&2; return 1 ;;
    esac
  elif [ "$out" = "release not found" ]; then
    echo none
  else
    echo "Couldn't look up $1's release on $repo: $out" >&2
    return 1
  fi
}
# The fork's default branch (main) doesn't have release-windows.yml, and gh run list --workflow
# only finds workflows on it, so runs are picked out by the tag and the workflow's name.
wf_name="Release (Windows)"
recut_hint() {
  echo "Don't skip it. Re-run the run's failed jobs (gh run list --repo $repo --branch $1)," >&2
  echo "or mark the release failed on the board (tb release fail R<n>) and cut it again: tb release cut $1" >&2
}

replace=
if echo "$versions" | grep -qxF -- "$version"; then
  state=$(release_state "$tag") || exit 1
  if [ "$state" = published ]; then
    echo "$tag is already published; a version is never reused. The next one is windows-v$(next_of "$base")." >&2
    exit 1
  fi
  if [ -z "${TB_RELEASE_REPLACE:-}" ]; then
    echo "$tag is already tagged, with no published release: its workflow failed, was cancelled or is still running." >&2
    recut_hint "$tag"
    exit 1
  fi
  # A run still going would publish the old commit, or fight the new run over the release.
  if ! running=$(gh run list --repo "$repo" --branch "$tag" --json status,url,workflowName \
      -q "[.[] | select(.workflowName == \"$wf_name\" and .status != \"completed\")][0].url // empty" 2>&1); then
    echo "Couldn't list $tag's workflow runs: $running" >&2
    exit 1
  fi
  if [ -n "$running" ]; then
    echo "$tag's release run is still going ($running). Cancel it (gh run cancel), then cut again." >&2
    exit 1
  fi
  # Out of the list, so the checks below treat it as the version being cut.
  replace=1
  versions=$(echo "$versions" | grep -vxF -- "$version" || true)
fi

if [ "$base" != "$files_base" ]; then
  echo "$tag is a build of $base, but the files say $files_base (scripts/set-version.sh --check)." >&2
  echo "The next Windows version of $files_base is windows-v$(next_of "$files_base")." >&2
  exit 1
fi

latest=
# shellcheck disable=SC2086
[ -z "$versions" ] || latest=$(newest $versions)
if [ -n "$latest" ] && [ "$(newest "$latest" "$version")" != "$version" ]; then
  echo "$tag isn't newer than the newest Windows tag, windows-v$latest." >&2
  exit 1
fi

next=$(next_of "$base")
if [ "$version" != "$next" ]; then
  echo "$tag skips a number: the next Windows build of $base is windows-v$next." >&2
  exit 1
fi

# Never skip a version: a tag whose release was never published gets cut again, not a new one.
if [ -n "$latest" ]; then
  state=$(release_state "windows-v$latest") || exit 1
  if [ "$state" != published ]; then
    [ "$state" = draft ] && what="only a draft" || what="no release at all"
    echo "windows-v$latest has no published release ($what): its workflow failed, was cancelled or is still running." >&2
    recut_hint "windows-v$latest"
    exit 1
  fi
fi

# The push the cut ends with, without making it: it has to get past sign-in on its own.
push_flags=()
[ -z "$replace" ] || push_flags=(--force)
if ! out=$(git push --dry-run --porcelain ${push_flags[@]+"${push_flags[@]}"} origin "HEAD:refs/tags/$tag" </dev/null 2>&1); then
  echo "$out" >&2
  echo "Pushing to origin needs a sign-in this script can't give. Push something to it by hand once (the credential manager keeps the sign-in), then run this again." >&2
  exit 1
fi

head=$(git rev-parse --short HEAD)
if [ -n "$dry_run" ]; then
  how="would tag"
  [ -z "$replace" ] || how="would move the tag to"
  echo "Dry run: $tag (a build of $base${latest:+, after windows-v$latest}) $how $branch_from at $head and push it to $repo."
  exit 0
fi

sha=$(git rev-parse HEAD)
# A minute's slack for this PC's clock against GitHub's.
since=$(date -u -d '-60 seconds' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v-60S +%Y-%m-%dT%H:%M:%SZ)
[ -z "$replace" ] || git tag -d "$tag" >/dev/null 2>&1 || true
git tag -a "$tag" -m "Taskboard $version for Windows"
if ! git push ${push_flags[@]+"${push_flags[@]}"} origin "refs/tags/$tag" </dev/null; then
  git tag -d "$tag" >/dev/null
  echo "Couldn't push $tag; the local tag is gone again, so cut it again once the push works." >&2
  exit 1
fi

echo
echo "Pushed $tag. The Windows release workflow builds it and publishes the release:"
workflow="https://github.com/$repo/actions?query=branch%3A$tag"
run=
for _ in $(seq 1 15); do
  # This push's run, not an earlier one of the same tag (a version cut again after a failure).
  run=$(gh run list --repo "$repo" --branch "$tag" --commit "$sha" --event push --json url,createdAt,workflowName \
    -q "[.[] | select(.workflowName == \"$wf_name\" and .createdAt >= \"$since\")][0].url // empty" 2>/dev/null || true)
  [ -z "$run" ] || break
  sleep 2
done
echo "  ${run:-$workflow}"
