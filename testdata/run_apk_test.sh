#!/usr/bin/env bash
# End-to-end APK test: build a real APK (binary AndroidManifest.xml + DEX),
# then launch its Activity through the manifest and check the output.
#
# This exercises the whole pipeline — ZIP reader, DEX parser, AXML parser,
# launcher-activity resolution, Activity instantiation and the interpreter.

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$HERE/build/apk"
R8="${APKNATIVE_D8:-$HOME/android-tools/r8.jar}"
BIN="${APKNATIVE_BIN:-$HERE/../target/release/apknative}"

for tool in "$R8" "$BIN" "$HERE/mkapk.py"; do
  [ -e "$tool" ] || { echo "missing $tool" >&2; exit 2; }
done

rm -rf "$WORK"
mkdir -p "$WORK"

PKG=com.example.demo
CLASS=DemoActivity

echo "== compiling $PKG.$CLASS =="
if ! javac -source 8 -target 8 -nowarn -d "$WORK" "$HERE/DemoActivity.java" 2>/dev/null; then
  echo "FAIL  javac"
  exit 1
fi
echo "== dexing =="
# d8 takes the class files; find them so the package layout doesn't matter.
find "$WORK" -name '*.class' -print0 | xargs -0 \
  java -cp "$R8" com.android.tools.r8.D8 --min-api 21 --output "$WORK" 2>/dev/null
[ -f "$WORK/classes.dex" ] || { echo "FAIL  d8 produced no classes.dex"; exit 1; }

echo "== packaging APK =="
python3 "$HERE/mkapk.py" "$WORK/classes.dex" "$WORK/demo.apk" "$PKG" "$CLASS" || exit 1

echo "== manifest =="
"$BIN" manifest "$WORK/demo.apk" | sed 's/^/  /'

echo "== launch =="
out=$(timeout 120 "$BIN" run "$WORK/demo.apk" 2>&1)
printf '%s\n' "$out" | sed 's/^/  /'

# The expectations below are the lines the Activity must print.
expected=(
  "DemoActivity.onCreate()"
  "sum 1..15 = 120"
  "len=9 upper=APKNATIVE"
  "part=apkn"
  "gcd(48,18) = 6"
  "0^2=0 1^2=1 2^2=4 3^2=9 4^2=16"
  "2^40 = 1099511627776"
  "activity finished"
)

fail=0
for e in "${expected[@]}"; do
  if grep -qF -- "$e" <<<"$out"; then
    echo "  ok   $e"
  else
    echo "  MISS $e"
    fail=$((fail + 1))
  fi
done

if [ "$fail" -eq 0 ]; then
  echo "=== APK launch: PASS ==="
else
  echo "=== APK launch: $fail missing ==="
fi
[ "$fail" -eq 0 ]