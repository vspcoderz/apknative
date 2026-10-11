#!/usr/bin/env bash
# Regression harness.
#
# For each test class: compile with javac, dex with Google's d8, run once on the
# real JVM (the reference answer) and once under apknative, then diff.
#
# Paths are derived from this script's location so it works both on the laptop
# and on the VPS. Override the d8 jar with APKNATIVE_D8 if it lives elsewhere.

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TD="$HERE"
WORK="$TD/build"
R8="${APKNATIVE_D8:-$HOME/android-tools/r8.jar}"
BIN="${APKNATIVE_BIN:-$HERE/../target/release/apknative}"

if [ ! -f "$R8" ]; then
  echo "d8 jar not found at $R8 (set APKNATIVE_D8)" >&2
  exit 2
fi
if [ ! -x "$BIN" ]; then
  echo "apknative binary not found at $BIN (set APKNATIVE_BIN)" >&2
  exit 2
fi

rm -rf "$WORK"
mkdir -p "$WORK"

pass=0
fail=0

for src in "$TD"/*.java; do
  name=$(basename "$src" .java)
  out="$WORK/$name"
  mkdir -p "$out"

  # Activity-style fixtures live in testdata/apk and are exercised by
  # run_apk_test.sh; they have no main().
  if ! grep -q 'static void main' "$src"; then
    echo "SKIP  $name (no main; covered by run_apk_test.sh)"
    continue
  fi

  if ! javac -source 8 -target 8 -nowarn -d "$out" "$src" 2>/dev/null; then
    echo "SKIP  $name (javac failed)"
    continue
  fi
  java -cp "$R8" com.android.tools.r8.D8 --min-api 21 --output "$out" "$out"/*.class 2>/dev/null

  ref=$(java -cp "$out" "$name" 2>&1)
  # Strip apknative's own banner lines, keeping only what the program printed.
  got=$(timeout 180 "$BIN" run "$out/classes.dex" 2>&1 \
        | sed -n '/--- program output ---/,/--- end ---/p' | sed '1d;$d')

  if [ "$ref" == "$got" ]; then
    echo "PASS  $name"
    pass=$((pass + 1))
  else
    echo "FAIL  $name"
    fail=$((fail + 1))
    echo "  expected:"; printf '%s\n' "$ref" | sed 's/^/    /'
    echo "  got:";      printf '%s\n' "$got"  | sed 's/^/    /'
  fi
done

echo "=== $pass passed, $fail failed ==="
[ "$fail" -eq 0 ]