#!/bin/bash
# The Windows side of scripts/install-app.sh (Taskboard) and scripts/dev-app.sh (Taskboard Dev):
# builds the install folder (packaging/windows/build.sh), puts it in %LOCALAPPDATA%\Programs\<name>
# and opens it. The app then adds its startup entry, link scheme, Start menu and Settings > Apps
# entries and tb's folder on the user's Path, and starts its daemon (crates/taskboard-app/src/win.rs).
# A release zip installs the same way with no script: its taskboard-app.exe copies itself in
# (crates/taskboard-app/src/win_install.rs).
#
#   packaging/windows/install.sh --real|--dev [--test] [--no-build] [--seed]
#   packaging/windows/install.sh --real|--dev --uninstall
#
# Taskboard Dev never touches Taskboard: its own id, startup entry, link scheme (taskboard-dev://),
# port (18792), data folder (%LOCALAPPDATA%\com.mrgnhnt.taskboard.dev\data, so its runner is off)
# and icon, and it leaves ~/.local/bin/tb.exe alone.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

KIND="${1:-}"
shift || true
LOCAL="$(cygpath -u "$LOCALAPPDATA")"
case "$KIND" in
  --real)
    ID="com.mrgnhnt.taskboard" NAME="Taskboard" SCHEME="taskboard" PORT=8792
    ICON="packaging/assets/Taskboard.ico" TARGET="$ROOT/target/package" OUT="$ROOT/dist/windows"
    DATA="$HOME/.config/taskboard" ENV=()
    ;;
  --dev)
    ID="com.mrgnhnt.taskboard.dev" NAME="Taskboard Dev" SCHEME="taskboard-dev" PORT=18792
    ICON="packaging/assets/TaskboardDev.ico" TARGET="$ROOT/target/dev-app" OUT="$ROOT/dist/windows-dev"
    DATA="$(cygpath -w "$LOCAL/$ID/data")"
    ENV=(--env "TASKBOARD_PAGE_URL=taskboard-dev://" --env "TASKBOARD_DATA=$DATA" --env "TASKBOARD_PORT=$PORT" --env "TASKBOARD_NO_TB_LINK=1")
    ;;
  *) echo "usage: $0 --real|--dev [--test] [--no-build] [--seed] [--uninstall]" >&2; exit 2 ;;
esac
DEST="$LOCAL/Programs/$NAME"

TEST=0 BUILD=1 UNINSTALL=0 SEED=0
for a in "$@"; do
  case "$a" in
    --test) TEST=1 ;;
    --no-build) BUILD=0 ;;
    --seed) SEED=1 ;;
    --uninstall) UNINSTALL=1 ;;
    *) echo "install.sh: unknown option $a" >&2; exit 2 ;;
  esac
done

if [ "$UNINSTALL" = 1 ]; then
  # Quits the app, stops the daemon, takes out the startup entry, link scheme, tb copy, Path entry,
  # Start menu and Settings > Apps entries and hooks (and deletes the folder itself, once it exits).
  [ -f "$DEST/taskboard-app.exe" ] && { "$DEST/taskboard-app.exe" --uninstall || true; }
  rm -rf "$DEST"
  echo "==> removed $DEST (data kept: $DATA)"
  exit 0
fi

if [ "$TEST" = 1 ]; then
  echo "==> cargo test"
  if ! cargo test --workspace; then
    echo "tests failed; not installing" >&2
    exit 1
  fi
fi

build=(packaging/windows/build.sh --out "$OUT" --target-dir "$TARGET" --id "$ID" --name "$NAME" --scheme "$SCHEME" --icon "$ICON" ${ENV[@]+"${ENV[@]}"})
[ "$BUILD" = 1 ] || build+=(--no-build)
echo "==> build $NAME"
"${build[@]}" 2>&1 | grep -E "==> built|^error|warning: unused" || true
[ -f "$OUT/$NAME/taskboard-app.exe" ] || { echo "no app at $OUT/$NAME (run without --no-build)" >&2; exit 1; }

if [ "$SEED" = 1 ]; then
  mkdir -p "$(cygpath -u "$DATA")"
  TASKBOARD_DATA="$DATA" "$OUT/$NAME/taskboardd.exe" seed --data "$DATA" || true
fi

# Copy next to the destination first, so the running copy is only stopped once the new one is ready.
NEW="$LOCAL/Programs/.$NAME.new" OLD="$LOCAL/Programs/.$NAME.old"
rm -rf "$NEW" "$OLD"
mkdir -p "$LOCAL/Programs"
cp -R "$OUT/$NAME" "$NEW"
if [ -f "$DEST/taskboard-app.exe" ]; then
  echo "==> stop the running $NAME and its daemon"
  "$DEST/taskboard-app.exe" --stop || echo "    it didn't stop cleanly" >&2
fi
if [ -e "$DEST" ] && ! mv "$DEST" "$OLD"; then
  rm -rf "$NEW"
  echo "something still runs from $DEST (an agent's tb.exe?); try again once it's done" >&2
  exit 1
fi
mv "$NEW" "$DEST"
rm -rf "$OLD" 2>/dev/null || echo "    $OLD is still in use; delete it later" >&2

echo "==> open $DEST"
cmd.exe //c start "" "$(cygpath -w "$DEST/taskboard-app.exe")"
for _ in $(seq 1 30); do
  sleep 0.5
  if curl -fsS "http://127.0.0.1:$PORT/tasks/api/state" >/dev/null 2>&1; then
    echo "    daemon answering on :$PORT"
    break
  fi
done
curl -fsS "http://127.0.0.1:$PORT/tasks/api/state" >/dev/null 2>&1 || echo "    its daemon isn't answering on :$PORT yet (see app.log in $DATA)" >&2
echo "==> done"
