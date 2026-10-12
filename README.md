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
| Dalvik interpreter (int, long, float, double) | working |
| Activity lifecycle (`onCreate`/`onStart`/`onResume`) | working |
| Static bytecode verifier | working |
| `java.util` collections, iterators, boxing | working |
| Compose composition (slot table, CompositionLocals, snapshots) | running |
| Compose rendering (node capture → measure → layout → raster) | not started |
| Android framework shims | partial |

A Compose app's screen composable now runs to completion without throwing.
F-Droid's `MainContent` executes; it emits into a Composer that records
nothing, so there is no tree to draw yet. See `PLAN.md` for the stages.

The test suite compiles Java, dexes it with Google's own `d8`, runs each program
on the real JVM and on `apknative`, and diffs the output. All five cases are
byte-identical:

```
$ ./testdata/run_tests.sh
PASS  Hello      # recursion, loops, StringBuilder, arrays, long arithmetic
PASS  Lit        # string concatenation
PASS  W
PASS  X
PASS  Refs       # null checks, identity, interface dispatch
=== 5 passed, 0 failed ===
```

## What actually runs

A production APK, end to end. `apknative` reads F-Droid's manifest, resolves
`org.fdroid.MainActivity` across three DEX files, allocates it, and drives the
real Android lifecycle:

```
$ apknative run ~/android-tools/apks/fdroid.apk
no main(); launching Activity: org.fdroid.MainActivity
(3 DEX files loaded)
apknative: instantiating org.fdroid.MainActivity
--- program output ---
--- end ---
```

That covers manifest parsing, multidex, method resolution across files,
AndroidX, Kotlin stdlib, Dagger/Hilt startup, and roughly a thousand
interpreted instructions of the app's own code. It is not a stub that returns
success — it executes.

## What does not

**No pixels.** F-Droid's UI is Jetpack Compose, and Compose composables write
into a `Composer` that a runtime walks to build a node tree, which a renderer
then draws. None of those exist here yet, so `setContent` is currently
answered as a no-op and the app finishes its lifecycle without a screen.

The framework is the constraint, and it is not an opcode problem — it is
~50,000 framework classes (`Activity`, `Context`, `Intent`, `ContentResolver`,
Binder IPC, `resources.arsc`, themes, layout, `PackageManager`). Google Play
Store is the extreme case: it ships as a split APK bundle depending on Google
Play Services, is R8-obfuscated, has native libraries for four ABIs, and
drives the system over Binder.

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