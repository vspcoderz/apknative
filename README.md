# apknative

Runs Android app bytecode on Linux **without an emulator**. No QEMU, no CPU
translation, no Android VM — it parses the APK and executes Dalvik directly in
Rust.

```
apknative run app.apk              # run a real APK
apknative run classes.dex          # run a bare DEX
apknative manifest app.apk         # decode AndroidManifest.xml
apknative info app.apk             # DEX statistics
apknative classes app.apk          # list classes and methods
apknative verify app.apk           # static-check all bytecode
```

## Status

| Area | State |
|---|---|
| ZIP/APK reader (DEFLATE, stored) | working |
| DEX parser (header, tables, class data, code items) | working |
| Binary XML / `AndroidManifest.xml` | working |
| Launcher-activity resolution (incl. aliases, `enabled=false`) | working |
| Multidex class location | working |
| Dalvik interpreter (~70 opcodes) | working |
| Static bytecode verifier | working |
| Android framework shims | minimal — the hard limit |

The test suite compiles Java, dexes it with Google's own `d8`, runs each program
on the real JVM and on `apknative`, and diffs the output. All four cases are
byte-identical:

```
$ ./testdata/run_tests.sh
PASS  Hello      # recursion, loops, StringBuilder, arrays, long arithmetic
PASS  Lit        # string concatenation
PASS  W
PASS  X
=== 4 passed, 0 failed ===
```

## What actually runs

Apps that do real computation with little Android API surface. Verified against
a production APK: `apknative` reads F-Droid's manifest, resolves
`org.fdroid.MainActivity`, allocates it, runs its constructor and enters real
Kotlin stdlib code.

## What does not

Anything that needs the Android framework. This is the real constraint, and it
is not an opcode problem — it is ~50,000 framework classes (`Activity`,
`Context`, `Intent`, `ContentResolver`, Binder IPC, `resources.arsc`, themes,
layout, `PackageManager`). Google Play Store is the extreme case: it ships as a
split APK bundle depending on Google Play Services, is R8-obfuscated, has native
libraries for four ABIs, and drives the system over Binder.

There is no shortcut here. The Android framework is the operating system, and
reimplementing it is a different project from reimplementing the bytecode.

## Architecture

```
APK (ZIP)
  └── classes.dex ──► dex.rs ──► Dalvik bytecode
                                      │
                        ┌─────────────┴─────────────┐
                        ▼                           ▼
                  verify.rs                   runtime.rs
                (static checks)              (interpreter + heap)
                                                    │
                                             shims for a few
                                             core classes
```

- `zip.rs` — ZIP central directory; inflates on demand
- `dex.rs` — DEX header, string/type/proto/field/method tables, class data
- `axml.rs` — Android binary XML, for the manifest
- `verify.rs` — walks every method checking widths, branch targets, registers
- `runtime.rs` — the interpreter, object heap, and built-in shims

### Notes on the bytecode

Several things here are easy to get wrong and are commented at their definitions:

- **`35c` calls do not name a destination register.** The result is stashed and
  placed by the following `move-result`, which names the target itself. Two
  otherwise-identical call sites in `Hello` write to different registers.
- **Instruction widths live in one table** (`insn_width`), not scattered through
  each opcode arm. Getting a width wrong desynchronises the whole stream.
- **`22b` packs a register in the low byte and a literal in the high byte** of
  its second unit.
- **A `long`/`double` parameter occupies two register slots**, so a method's
  `ins_size` is a word count and can exceed its parameter count.
- **Kotlin overloads can share an arity** (`coerceAtLeast(I,I)` vs
  `coerceAtLeast(D,D)`), so calls are matched on full signature, not name.

## Building

Local:

```sh
cargo build --release
./testdata/run_tests.sh
```

On the VPS (key-only SSH; no password is stored):

```sh
./vps.sh build     # sync + cargo build --release
./vps.sh test      # sync + regression suite
./vps.sh fetch     # pull the release binary back
```

## Validation

`apknative verify` exists because finding decoder bugs one runtime panic at a
time against a real APK does not scale. It walks every method up front and
reports width overruns, branches that miss instruction boundaries, and register
accesses outside the frame — which is how the `22c`/`22b`/`35c` bugs above were
actually found.