#!/bin/bash
# Build the Windows release folder (and, when asked, the release zip). Runs in Git Bash, here and on
# GitHub's Windows runners; the macOS app is packaging/build-app.sh.
#
#   packaging/windows/build.sh [options]
#     --version V         version for the install (default: workspace Cargo version)
#     --out DIR           output directory (default: dist/<version>); the install is DIR/<name>
#     --no-build          reuse the binaries from the last build (same --target-dir)
#     --target-dir DIR    cargo target dir (default: target/package, kept apart from dev builds)
#     --id ID             the install's id (default com.mrgnhnt.taskboard); the updater only replaces this one
#     --label LABEL       the daemon's label (default <id>.daemon): names its supervisor
#     --name NAME         what Windows shows, and the folder's name (default Taskboard)
#     --icon FILE         .ico built into the app (default packaging/assets/Taskboard.ico)
#     --scheme NAME       link scheme the app opens (default taskboard: taskboard://task/T12)
#     --env K=V           the app's and the daemon's environment (e.g. TASKBOARD_DATA=…,
#                         TASKBOARD_PORT=18792); repeatable
#     --zip               also DIR/<name>-<version>-windows-x64.zip: what people download, and the
#                         updater too
#
# The folder holds taskboard-app.exe (the window, and with --daemon the daemon's supervisor),
# taskboardd.exe (the board), tb.exe (the agent CLI; the app keeps a copy in ~/.local/bin and puts
# that on the user's Path), the Claude Code plugin, the fonts' licenses, install.json, which says
# the above (crates/taskboard-app/src/win.rs), and taskboard-bundle.txt. There's no installer:
# taskboard-app.exe run from the folder (unzipped anywhere) copies it into
# %LOCALAPPDATA%\Programs\<name> and starts that copy (crates/taskboard-app/src/win_install.rs).
# The marker is what lets it: a taskboard-app.exe without one never installs itself.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

VERSION="" OUT="" BUILD=1 TARGET_DIR="$ROOT/target/package" ID="com.mrgnhnt.taskboard" LABEL="" NAME="Taskboard"
ICON="" SCHEME="taskboard" EXTRA_ENV=() ZIP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --no-build) BUILD=0; shift ;;
    --target-dir) TARGET_DIR="$2"; shift 2 ;;
    --id) ID="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --icon) ICON="$2"; shift 2 ;;
    --scheme) SCHEME="$2"; shift 2 ;;
    --env) EXTRA_ENV+=("$2"); shift 2 ;;
    --zip) ZIP=1; shift ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) echo "build.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

LABEL="${LABEL:-$ID.daemon}"
if [ -z "$VERSION" ]; then
  VERSION="$(awk '/^\[workspace.package\]/{p=1} p && /^version *=/{gsub(/[" ]/,"",$0); sub(/version=/,""); print; exit}' Cargo.toml)"
fi
OUT="${OUT:-$ROOT/dist/$VERSION}"
case "$OUT" in /*|[A-Za-z]:*) ;; *) OUT="$ROOT/$OUT" ;; esac
DIR="$OUT/$NAME"

# ---------------------------------------------------------------- build
BIN="$TARGET_DIR/release"
if [ "$BUILD" = 1 ]; then
  echo "==> cargo build --release ($NAME $VERSION, target $TARGET_DIR)"
  icon_env=()
  [ -n "$ICON" ] && icon_env=(TASKBOARD_ICON="$(cygpath -w "$(cd "$(dirname "$ICON")" && pwd)/$(basename "$ICON")")")
  # RUSTFLAGS would replace .cargo/config.toml's rustflags, and with them the static C runtime a
  # clean Windows needs (check-runtime.sh, below, catches a build without it).
  unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
  env ${icon_env[@]+"${icon_env[@]}"} CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build --release -p taskboard-app -p taskboardd -p taskboard-cli --bin taskboard-app --bin taskboardd --bin tb
fi
for b in taskboard-app taskboardd tb; do [ -f "$BIN/$b.exe" ] || { echo "missing $BIN/$b.exe" >&2; exit 1; }; done

# ---------------------------------------------------------------- assemble
echo "==> assembling $DIR"
rm -rf "$DIR"
mkdir -p "$DIR/Fonts"
cp "$BIN/taskboard-app.exe" "$BIN/taskboardd.exe" "$BIN/tb.exe" "$DIR/"
# The fonts are compiled into taskboard-app (include_bytes!); their OFL licenses travel with it.
cp crates/taskboard-app/assets/fonts/OFL-*.txt "$DIR/Fonts/"
# The Claude Code plugin (hooks, skill, tb shim, Midna's status-bar script). Claude Code loads it in
# place, so updating the app updates it.
cp -R plugin "$DIR/plugin"

json_str() {  # a JSON string: backslashes and quotes escaped (Windows paths)
  local s="${1//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '"%s"' "$s"
}
{
  printf '{\n'
  printf '  "id": %s,\n' "$(json_str "$ID")"
  printf '  "name": %s,\n' "$(json_str "$NAME")"
  printf '  "scheme": %s,\n' "$(json_str "$SCHEME")"
  printf '  "label": %s,\n' "$(json_str "$LABEL")"
  printf '  "version": %s,\n' "$(json_str "$VERSION")"
  printf '  "env": {'
  sep=""
  for kv in ${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"}; do
    printf '%s\n    %s: %s' "$sep" "$(json_str "${kv%%=*}")" "$(json_str "${kv#*=}")"
    sep=","
  done
  [ -n "$sep" ] && printf '\n  '
  printf '}\n}\n'
} > "$DIR/install.json"
python -c 'import json,sys; json.load(open(sys.argv[1], encoding="utf-8"))' "$DIR/install.json" 2>/dev/null \
  || python3 -c 'import json,sys; json.load(open(sys.argv[1], encoding="utf-8"))' "$DIR/install.json"
# The marker (win_install::MARKER): with it beside taskboard-app.exe, the folder installs itself.
cat > "$DIR/taskboard-bundle.txt" <<EOF
$NAME $VERSION. With this file beside it, taskboard-app.exe installs this folder into
%LOCALAPPDATA%\Programs\\$NAME when it's run from anywhere else, then starts that copy.
EOF
packaging/windows/check-runtime.sh "$DIR"
echo "==> built $DIR ($VERSION, $(du -sh "$DIR" | cut -f1))"

# ---------------------------------------------------------------- update zip
if [ "$ZIP" = 1 ]; then
  zip="$OUT/$NAME-$VERSION-windows-x64.zip"
  rm -f "$zip"
  # Windows' own tar writes zips (-a picks the format from the name), and the updater unpacks with it.
  tar="$(cygpath -u "$SYSTEMROOT")/System32/tar.exe"
  (cd "$OUT" && "$tar" -a -c -f "$(cygpath -w "$zip")" "$NAME")
  listing="$("$tar" -t -f "$(cygpath -w "$zip")")"
  for f in install.json taskboard-bundle.txt taskboard-app.exe; do
    grep -qx "$NAME/$f" <<<"$listing" || { echo "$zip has no $NAME/$f" >&2; exit 1; }
  done
  echo "==> $zip"
fi
