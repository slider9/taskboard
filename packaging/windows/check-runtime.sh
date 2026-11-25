#!/bin/bash
# Check that a Taskboard for Windows install folder (packaging/windows/build.sh) starts on a clean
# Windows 10, which has no Visual C++ redistributable: fails when an .exe or .dll in it imports one
# of the redistributable's DLLs (VCRUNTIME140.dll, MSVCP140.dll and the rest), or the Universal CRT
# (api-ms-win-crt-*.dll, ucrtbase.dll), which means the static C runtime (+crt-static,
# .cargo/config.toml) didn't take.
#
#   packaging/windows/check-runtime.sh DIR
#
# Runs in Git Bash; needs dumpbin.exe, from Visual Studio's C++ build tools (found with vswhere, or
# on PATH).
set -euo pipefail

DIR="${1:-}"
[ -n "$DIR" ] && [ -d "$DIR" ] || { echo "usage: check-runtime.sh DIR (a Taskboard for Windows folder)" >&2; exit 2; }

DUMPBIN="$(command -v dumpbin.exe || true)"
if [ -z "$DUMPBIN" ]; then
  # Where the Visual Studio installer always puts it (%ProgramFiles(x86)%).
  vswhere="/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
  if [ -x "$vswhere" ]; then
    DUMPBIN="$("$vswhere" -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 \
      -find 'VC/Tools/MSVC/**/bin/Hostx64/x64/dumpbin.exe' | tr -d '\r' | head -n 1)"
  fi
fi
[ -n "$DUMPBIN" ] || { echo "check-runtime.sh: no dumpbin.exe (install Visual Studio's C++ build tools)" >&2; exit 2; }

PATTERN='^((vcruntime|msvcp|concrt|vccorlib|vcomp|vcamp|mfc)[0-9_a-z]*|api-ms-win-crt-.*|ucrtbase)\.dll$'

bad=0 checked=0
shopt -s nullglob nocaseglob
for f in "$DIR"/*.exe "$DIR"/*.dll; do
  name="$(basename "$f")"
  # dumpbin lists each import (and delay-load import) indented on a line of its own.
  deps="$("$DUMPBIN" -nologo -dependents "$(cygpath -w "$f")" | tr -d '\r' | sed -n 's/^ \{4\}\([^ ]*\.[dD][lL][lL]\)$/\1/p')"
  [ -n "$deps" ] || { echo "check-runtime.sh: dumpbin found no imports in $name" >&2; exit 1; }
  hits="$(tr 'A-Z' 'a-z' <<<"$deps" | grep -E "$PATTERN" || true)"
  if [ -n "$hits" ]; then
    echo "check-runtime.sh: $name imports C runtime DLLs a clean Windows doesn't have:" $hits >&2
    bad=1
  fi
  checked=$((checked + 1))
done
[ "$checked" -gt 0 ] || { echo "check-runtime.sh: no .exe or .dll in $DIR" >&2; exit 1; }
[ "$bad" = 0 ] || exit 1
echo "==> $checked files in $DIR need no Visual C++ runtime"
