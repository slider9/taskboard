#!/bin/bash
# Build the Windows install folder (and, when asked, the update zip and the installer). Runs in Git
# Bash, here and on GitHub's Windows runners; the macOS app is packaging/build-app.sh.
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
#     --zip               also DIR/<name>-<version>-windows-x64.zip, what the updater downloads
#     --installer         also DIR/<name>-<version>-windows-x64-setup.exe (needs Inno Setup 6's ISCC)
#
# The install folder holds taskboard-app.exe (the window, and with --daemon the daemon's
# supervisor), taskboardd.exe (the board), tb.exe (the agent CLI; the app keeps a copy in
# ~/.local/bin), the Claude Code plugin, the fonts' licenses and install.json, which says the above
# (crates/taskboard-app/src/win.rs). The installer puts it in %LOCALAPPDATA%\Programs\<name>.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

VERSION="" OUT="" BUILD=1 TARGET_DIR="$ROOT/target/package" ID="com.mrgnhnt.taskboard" LABEL="" NAME="Taskboard"
ICON="" SCHEME="taskboard" EXTRA_ENV=() ZIP=0 INSTALLER=0
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
    --installer) INSTALLER=1; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
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
echo "==> built $DIR ($VERSION, $(du -sh "$DIR" | cut -f1))"

# ---------------------------------------------------------------- update zip
if [ "$ZIP" = 1 ]; then
  zip="$OUT/$NAME-$VERSION-windows-x64.zip"
  rm -f "$zip"
  # Windows' own tar writes zips (-a picks the format from the name), and the updater unpacks with it.
  tar="$(cygpath -u "$SYSTEMROOT")/System32/tar.exe"
  (cd "$OUT" && "$tar" -a -c -f "$(cygpath -w "$zip")" "$NAME")
  "$tar" -t -f "$(cygpath -w "$zip")" | grep -qx "$NAME/install.json"
  echo "==> $zip"
fi

# ---------------------------------------------------------------- installer
if [ "$INSTALLER" = 1 ]; then
  iscc="$(command -v iscc || command -v ISCC || true)"
  for c in "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" "/c/Program Files/Inno Setup 6/ISCC.exe" "$LOCALAPPDATA/Programs/Inno Setup 6/ISCC.exe"; do
    [ -z "$iscc" ] && [ -f "$c" ] && iscc="$c"
  done
  [ -n "$iscc" ] || { echo "no Inno Setup 6 (ISCC.exe): install it, or build without --installer" >&2; exit 1; }
  # Windows wants a numeric file version: 1.2.0-beta.3 -> 1.2.0.
  "$iscc" -Q "-DVersion=$VERSION" "-DFileVersion=${VERSION%%-*}" "-DName=$NAME" "-DSource=$(cygpath -w "$DIR")" \
    "-DOutputDir=$(cygpath -w "$OUT")" "-DOutputName=$NAME-$VERSION-windows-x64-setup" "$(cygpath -w packaging/windows/Taskboard.iss)"
  echo "==> $OUT/$NAME-$VERSION-windows-x64-setup.exe"
fi
