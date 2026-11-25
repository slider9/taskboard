# Releasing Taskboard

Taskboard ships the way Midna does: a version tag builds, signs and notarizes the app in CI,
packs it in a DMG and publishes a GitHub release. The website (`site/`, taskboard.mrgnhnt.com)
points its Download button at the newest release with a DMG, betas included.

Windows releases ship from the fork (`slider9/taskboard`) with a tag and a workflow of their own:
see "Windows releases" at the end.

## Cut a release

Run it in a checkout of `main` (a beta can also go from a `release/*` branch) with nothing uncommitted:

```sh
tb release cut v1.1.0          # public release, from main
tb release cut v1.2.0-beta.1   # beta (prerelease)
```

`tb release cut` refuses any other branch, fast-forwards the checkout to origin's copy of it, then
runs the project's release script, `scripts/release.sh`, with the name. It records the release on
the board, holding every done task in Unreleased, only if the script exits 0, and prints the
release's workflow run once GitHub has started it. With no done task in Unreleased it refuses
before running the script.

You need:

- the project set up for releases, once: `tb project set taskboard --releases on --release-script
  scripts/release.sh`. Without a release script, `tb release cut` only records the release on the board: no
  version bump, tag or CI run.
- the release deploy key at `~/.ssh/taskboard-release` (or `TB_RELEASE_DEPLOY_KEY`), because
  `main` only takes PRs and the release commit is the one direct push. README.md, "Developing",
  says how to make it.
- `gh`, signed in, when a failed release is cut again (it deletes the failed run's GitHub release).

Don't run `scripts/release.sh` yourself: without `TB_RELEASE` (which `tb release cut` sets) it
stops and tells you to run `tb release cut`. `--force` runs it without the board, for when the
board is down, on the same branches (`main`, or `release/*` for a beta); record the release
afterwards with `tb release cut <name> --no-script`.

`release.sh` checks the version is newer than the last one (`scripts/newest-version.py`): a public
release than the last public one, a beta than every tag. It writes the version into
`Cargo.toml`/`Cargo.lock` (`scripts/set-version.sh`), sets the Claude Code plugin's version from a
hash of its files (`scripts/plugin-version.sh`), commits, tags `vX.Y.Z` and pushes the branch and
tag together.

The tag runs `.github/workflows/release.yml` on a `macos-15` runner, which:

1. checks the files' version matches the tag, and the plugin's matches its files
2. imports the Developer ID certificate into a throwaway keychain
3. `packaging/build-app.sh` (Developer ID signed, hardened runtime)
4. `packaging/notarize.sh` (App Store Connect API key), staples
5. packs the stapled app as `Taskboard-<version>.app.tar.gz`, for the app's updater
6. `scripts/build-dmg.sh` (branded window, `packaging/assets/dmg`), signs, notarizes and staples
   the DMG
7. publishes the GitHub release with `Taskboard-<version>-macos-arm64.dmg`, the `.app.tar.gz`
   and generated notes (betas as prereleases)

## When a release fails

If `release.sh` stops (wrong branch, a dirty tree, an old version, a rejected push), nothing is
recorded: fix what it says and run `tb release cut` again with the same name.

If the workflow fails after the tag is pushed, mark the release failed, then cut the same name
again:

```sh
tb release fail R12
tb release cut v1.1.0
```

`tb release fail` puts the release's tasks back in Unreleased and frees its name. The next cut of
that name sets `TB_RELEASE_REPLACE`, so `release.sh` moves the tag to `HEAD`, deletes any GitHub
release the failed run published, and force-pushes the tag, which starts the workflow again.

## Updates

The app updates itself from the release's `.app.tar.gz` (`crates/taskboard-app/src/updater.rs`):
it checks the archive against GitHub's sha256 for it, unpacks it, checks the app's signature,
notarization and team, and swaps it in. A release without the archive isn't offered as an update.
It used to install from the DMG, but mounting and unmounting a fresh DMG took about 14 seconds.

A `windows-2022` job runs alongside, with the files checked out as they are in the repo (LF). It
checks the version, then `packaging/windows/build.sh --zip --installer` builds:

- `Taskboard-<version>-windows-x64-setup.exe`, the installer (Inno Setup,
  `packaging/windows/Taskboard.iss`). It's per user and needs no admin: the app goes in
  `%LOCALAPPDATA%\Programs\Taskboard`, with a Start menu entry and an uninstaller.
- `Taskboard-<version>-windows-x64.zip`, the same install folder, for the updater. It checks the zip
  against GitHub's sha256, then that its `install.json` names the app and the release's version, and
  swaps the files in.

Neither is code-signed yet, so Windows SmartScreen warns about the installer the first time.
Once releases are signed, an install updates only to programs signed by the same publisher.

A last job publishes the release with the macOS and Windows files.

## Repository secrets and variables

The same Apple credentials as Midna's release workflow.

| Name | Kind | What |
|---|---|---|
| `DEVELOPER_ID_CERTIFICATE` | secret | base64 of the Developer ID Application `.p12` |
| `DEVELOPER_ID_CERTIFICATE_PASSWORD` | secret | its password |
| `APP_STORE_CONNECT_API_KEY` | secret | contents of the `AuthKey_<id>.p8` (notarization) |
| `DEVELOPER_ID_IDENTITY` | variable | `Developer ID Application: Morgan Hunt (U2G2XV3688)` |
| `APP_STORE_CONNECT_KEY_ID` / `APP_STORE_CONNECT_ISSUER_ID` | variable | the API key's ids |

## By hand (no CI)

The same steps as the workflow, for a build when CI can't run:

```sh
packaging/build-app.sh --version 1.1.0 --out dist/1.1.0   # Developer ID signed when the cert is in the keychain
packaging/notarize.sh dist/1.1.0/Taskboard.app            # OPT-IN: uploads to Apple, then staples
(cd dist/1.1.0 && COPYFILE_DISABLE=1 tar -czf ../../Taskboard-1.1.0.app.tar.gz Taskboard.app)
./scripts/build-dmg.sh dist/1.1.0/Taskboard.app Taskboard-1.1.0-macos-arm64.dmg
codesign --force --timestamp --sign "Developer ID Application: Morgan Hunt (U2G2XV3688)" Taskboard-1.1.0-macos-arm64.dmg
xcrun notarytool submit Taskboard-1.1.0-macos-arm64.dmg --keychain-profile taskboard-notary --wait
xcrun stapler staple Taskboard-1.1.0-macos-arm64.dmg
gh release create v1.1.0 Taskboard-1.1.0-macos-arm64.dmg Taskboard-1.1.0.app.tar.gz \
  --title "Taskboard 1.1.0" --generate-notes --latest   # a beta: --prerelease --latest=false
```

`COPYFILE_DISABLE=1` keeps AppleDouble `._` files out of the archive; they break the signature.
Build from a commit whose version files already say 1.1.0 (`scripts/set-version.sh`), or the
app reports the wrong version. Without a `v1.1.0` tag, `gh release create` tags the default
branch's latest commit. `notarize.sh` uses the
keychain profile `taskboard-notary` locally
(`xcrun notarytool store-credentials taskboard-notary --apple-id … --team-id U2G2XV3688`).

On Windows, in Git Bash (`--installer` needs Inno Setup 6):

```sh
packaging/windows/build.sh --version 1.1.0 --out dist/1.1.0 --zip --installer
```

`scripts/install-app.sh` and `scripts/dev-app.sh` work on Windows too: they build the install
folder and put it in `%LOCALAPPDATA%\Programs\Taskboard` (or `Taskboard Dev`) without the installer.

## DMG window

`scripts/dmg-settings.py` places the icons; `packaging/assets/dmg/background.svg` is the
background. After changing it, run `scripts/render-dmg-background.sh` (ImageMagick) and commit
`background.tiff`.

## Website

`site/` is one static page, deployed to GitHub Pages by `.github/workflows/site.yml` on pushes
to `main` that touch it. `site/CNAME` holds the domain.

## Windows releases

Windows builds ship from the fork, `slider9/taskboard`, and only from `feat/windows-native`,
betas and full releases alike. Only slider9 runs them (the workflow checks `github.actor`), and
nothing is code-signed yet.

```sh
tb release cut windows-v0.1.0-beta.97.w.1        # a build of a beta base (a prerelease)
tb release cut windows-v0.2.0-w.1                # a build of a stable base
scripts/release-windows.sh 0.1.0-beta.97.w.1 --dry-run   # every check below, no tag
```

On slider9's board, taskboard's release script is `scripts/release-windows.sh`, from
`feat/windows-native`:

```sh
tb project set taskboard --releases on --release-script scripts/release-windows.sh --release-branch feat/windows-native
```

So `tb release cut` there cuts Windows releases only; macOS releases are upstream's
(`release.sh`, from `mrgnhnt96/taskboard`). The script takes `windows-v<version>`,
`v<version>` or `<version>`. Run on its own it refuses unless you pass `--force` (or
`--dry-run`, which runs every check and stops before the tag). Unlike `release.sh` it never
writes a version or commits: the files keep the upstream base and the counter lives only in the
tag (below), so a cut is an annotated tag on `feat/windows-native`'s head, pushed to `origin` on
its own. The push uses this PC's git sign-in for github.com (slider9's, in the credential
manager) with prompts turned off, because the board runs the script in the background; it prints
the run's link when it's done.

It refuses, and cuts nothing, when:

| It says | Why, and what to do |
|---|---|
| `Windows releases go out from feat/windows-native` | Betas and full releases alike come from that branch. Switch to it. |
| `Commit or stash your changes first` | The tag would leave out what's in the working tree. |
| `feat/windows-native isn't the same as origin's` | The tag goes on the commit everyone has: pull (or push) first. |
| `The plugin's version doesn't match its files` | Claude Code reloads the plugin only when its version moves, and this script never commits: run `scripts/plugin-version.sh` in a PR, merge it, cut again. |
| `… is already published; a version is never reused` | Installed copies may have it. Cut the next one, which it prints. |
| `… is already tagged, with no published release` | Its workflow failed, was cancelled or is still running. Don't skip it: see "A Windows release failed". |
| `… is a build of X, but the files say Y` | The version's base must be what `scripts/set-version.sh --check` says `Cargo.toml` has; it prints the next version of that base. |
| `… isn't newer than the newest Windows tag` | Updates never go backwards. |
| `… skips a number` | The counter is the next one after the newest Windows tag of the same base, or 1 for a new base; it prints which. |
| `… has no published release (none, or only a draft)` | The newest Windows tag never went out. Don't move past it: see "A Windows release failed". |
| `Pushing to origin needs a sign-in this script can't give` | The push would stop at a prompt. Push something to `origin` by hand once so the credential manager keeps the sign-in, then cut again. |

It looks only at `windows-v*` tags, here and on `origin`; bare `v` tags are macOS's and never
count. A tag that is only in this checkout counts too, so a tag deleted from `origin` has to be
deleted here as well (`git tag -d`).

### Version

A Windows version is the upstream version the fork last merged, which is what `Cargo.toml` says,
plus a Windows counter:

| Upstream base | Windows builds |
|---|---|
| `0.1.0-beta.97` (a beta) | `0.1.0-beta.97.w.1`, `0.1.0-beta.97.w.2`, … |
| `0.2.0` (stable) | `0.2.0-w.1`, `0.2.0-w.2`, … |

The files keep the upstream version, so a merge from upstream never conflicts on it; the counter
lives only in the tag. The counter restarts at 1 for each new base. By semver
`0.1.0-beta.97 < 0.1.0-beta.97.w.1 < 0.1.0-beta.97.w.2 < 0.1.0-beta.98 < 0.2.0-w.1 < 0.2.0`:
more parts win when the rest match, numbers compare as numbers, and `beta` sorts before `w`.
`0.2.0-w.1` is a semver pre-release of `0.2.0`, which is fine because no Windows build is ever
plain `0.2.0`; it still counts as a stable build here (it has no `-beta.`), so it is published as
the latest release. Build metadata (`0.2.0+w.1`) wouldn't work: semver ignores it, so `+w.2` would
never be newer. `scripts/newest-version.py` only knows upstream's versions, so the script orders
Windows ones itself.

### The workflow

A `windows-v` tag runs `.github/workflows/release-windows.yml`; a bare `v` tag runs `release.yml`
(macOS) and never this one, and `release.yml` never runs for a `windows-v` tag (its `v*` filter
doesn't match it). Two jobs, so a failed publish doesn't cost a rebuild:

- **build** (`windows-2022`, the files checked out as they are in the repo, LF)
  1. checks the tag's version and that the files say its upstream base
     (`scripts/set-version.sh --check <base>`), and that the plugin's version matches its files
  2. `packaging/windows/build.sh --version <v> --zip --installer` (Inno Setup 6), with
     `TASKBOARD_BUILD_VERSION=<v>` in cargo's environment, and checks `taskboard-app.exe` has
     `<v>` compiled in and `install.json` says `<v>`. A build that reported the base would see
     its own release as newer and update to it forever.
- **publish** (`ubuntu-latest`)
  3. publishes the GitHub release `windows-v<v>`, "Taskboard `<v>` for Windows", with
     `Taskboard-<v>-windows-x64-setup.exe` (the installer) and `Taskboard-<v>-windows-x64.zip`
     (what the updater downloads). A build of a beta base is a prerelease; a build of a stable
     base is the latest release. A re-run that finds the release already made replaces its files.

**What the updater reads.** A Windows Taskboard checks the fork's releases, not upstream's: a
beta build takes prereleases too, a stable build only full releases, and the newest `windows-v`
release with a `-windows-x64.zip` newer than the running build is the update. It checks the zip
against the sha256 GitHub keeps for the asset and that the zip's `install.json` names the app and
the release's version ("Updates", above).

**Secrets.** None. The workflow builds and publishes with `GITHUB_TOKEN` (`contents: write`), and
the fork has no ruleset, so it needs no deploy key; the tag push is the only thing done with
slider9's own credentials. Nothing is Authenticode-signed yet, so SmartScreen warns about the
installer the first time; once releases are signed, an install updates only to programs signed
by the same publisher.

**Trying it without a release.** Run the workflow by hand on a branch with a version, and it
makes a **draft** release with the installer and the zip:

```sh
gh workflow run release-windows.yml --repo slider9/taskboard --ref <branch> -f version=0.1.0-beta.97.w.1 -f draft=true
gh release download windows-v0.1.0-beta.97.w.1 --repo slider9/taskboard --pattern '*' --dir /tmp/rw   # drafts too
gh release delete windows-v0.1.0-beta.97.w.1 --repo slider9/taskboard --yes
```

GitHub finds a workflow by its file name only on the default branch, and the fork's is `main`,
which doesn't have this one. Once a tag has run it, `gh workflow list --repo slider9/taskboard
--all` gives its id, which `gh workflow run <id>` takes instead. Run by hand on a `windows-v` tag
with `draft` off, it publishes that tag like a push would; on a branch it only ever makes drafts.
A draft run refuses a version that is already published, and a real run replaces a leftover
draft of its version with a fresh release. Drafts are never offered as updates.

### A Windows release failed

**Never skip a version.** If the workflow fails or is cancelled before the release is published,
nobody has that version, so it goes out again at the same number (`release-windows.sh` refuses a
newer one until it has):

1. A flaky step (a runner hiccup, a download): **Re-run failed jobs** on the run. The build's
   files are kept as an artifact for 7 days, so a failed publish re-runs without a rebuild. Stop
   there.
2. Something to fix in the code: cancel any run still going (`gh run cancel <id>`), merge the
   fix into `feat/windows-native`, then mark the release failed and cut the same name again:

   ```sh
   tb release fail R12
   tb release cut windows-v0.1.0-beta.97.w.1
   ```

   The board sets `TB_RELEASE_REPLACE` for a name it has marked failed, so the script moves the
   tag to the new head and force-pushes it, which runs the workflow again. It still refuses while
   a run of that tag is going, and it never moves the tag of a published release. Without the
   board: `git push origin :refs/tags/windows-vX && git tag -d windows-vX`, then
   `scripts/release-windows.sh windows-vX --force`.

A draft on a `windows-v` tag (a hand run with `draft` on, or a publish that failed halfway)
counts as no release, so the script refuses to cut past it: publish it (run the workflow on the
tag with `draft` off), or cut it again as above. Once a release *is* published its number is
used up, because installed copies may already have it.

### By hand

The same build as the workflow, in Git Bash (`--installer` needs Inno Setup 6):

```sh
TASKBOARD_BUILD_VERSION=0.1.0-beta.97.w.1 packaging/windows/build.sh --version 0.1.0-beta.97.w.1 --out dist/w --zip --installer
```
