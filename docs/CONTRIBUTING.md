# Contributing

## Build and test

```
cargo build --workspace
cargo test --workspace
cargo build --release
```

Two binaries are produced in `target\release`: `Shell.exe` and `shellctl.exe`.

### If cargo builds something that cannot possibly be true

On this machine the guest clock jumps backwards, which makes a source file's
modification time older than the artefact built from it. Cargo then considers it
fresh and skips it, so you get errors that contradict the source in front of you:

```
error[E0559]: variant `ElementKind::Panel` has no field named `reveal`
```

...from a struct that plainly has that field. The same skew produces linker
failures that name an anonymous constant in a file nobody touched:

```
LNK2019: unresolved external symbol anon.0a9d0c74... referenced in
         function _RNv...slowshell_win8ipc_pipe11accept_loop
```

Both are stale artefacts, not real. Touching the sources is not always enough,
because the skew can survive that. The fix that always works is to drop the crate
you edited:

```
cargo clean -p slowshell-ui
cargo build --workspace --all-targets
```

The tell is a build that "Finished in 0.08s" after you edited a file, or an
error pointing at a line that does not exist. Trust the file, not the compiler,
until you have tried `cargo clean -p`.

Always build `--all-targets` when changing a struct: `cargo build` does not
compile `#[cfg(test)]` code, so a missing field in a test helper looks like a
clean build and then fails every subsequent `cargo test`.

Cargo aliases in `.cargo/config.toml` cover the awkward part — the workspace has
two binaries, so bare `cargo run` cannot work:

| | |
|---|---|
| `cargo shell` | run the shell (does not return; it is a shell) |
| `cargo ctl -- <command>` | run `shellctl` |
| `cargo validate -- <config>` | compile a config and report, then exit |
| `cargo run-shell` / `cargo run-ctl` | foreground debug builds |
| `cargo pixels` | the pixel-level render test |

`cargo build` (bare) still builds the whole workspace, because `default-members`
is deliberately not set — narrowing it would make `cargo build --release` skip
`shellctl`, which the README promises builds both binaries.

If cargo reports `Access is denied` on `Shell.exe`, a shell is already running
and the linker cannot replace it. `cargo ctl -- stop`, or
`Get-Process Shell | Stop-Process -Force`.

Requirements: a stable Rust toolchain (1.80 or newer) and Windows 10 or 11.
There is nothing else to install — no C++ build tools, no SDK, no runtime.


The whole workspace is five crates and three dependencies (`windows`, `serde`,
`serde_json`). If a change makes that number go up, that is worth explaining in
the commit message.

---

## Layout

| Crate | Owns | Depends on |
|---|---|---|
| `slowshell-core` | Language, reactivity, diagnostics, logging, IPC protocol, action registry | nothing |
| `slowshell-win` | Win32, Direct2D, DirectWrite, pipes, clipboard, processes | `core` |
| `slowshell-ui` | Element tree, style, layout, paint | `core`, `win` |
| `slowshell-runtime` | Loader, compiler, providers | all three |
| `slowshell` | `Shell.exe`: window loop, frame loop, surfaces | all four |
| `shellctl` | `shellctl.exe`: argument parsing, output | `core`, `win`, `runtime` |

**`slowshell-core` must not gain a Windows dependency.** That constraint is what
lets the language be tested without a desktop, and it is why `--check` can
validate a config without starting a shell. If a change needs a platform call,
it belongs in `slowshell-win`.

---

## House rules

These are the decisions that were made deliberately. Breaking one without a
reason in the commit message will be asked about.

### Verify by looking, not by reasoning

Ten of the bugs found during development were found by rendering and reading
pixels, not by reading code. Every one of them was obvious in the source and
invisible in the source at the same time:

- DirectWrite metrics divided by DPI twice, so every glyph was oversized at 125%
- `edges_of` not reading `Expr::List`, so `padding: [0, 12]` was silently ignored
- a full-style inheritance that double-counted padding
- layout dividing bounds by DPI instead of scale
- every `PropKind::Color` written to `style.foreground`
- a `Clock` measured as an empty string, collapsing the bar
- the reactor's `pending` never drained, so the shell ran at 55 fps forever

`crates/slowshell/examples/render_probe.rs` is the harness: it draws a known
scene, reads the pixels back, and asserts on fills, circles, strokes, text and
every font weight. Run it after touching the renderer.

```
cargo run -p slowshell --example render_probe
```

### Report rather than swallow

A failed action, an unknown provider, a broken element, a drop of a text run —
each is logged, with the reason. The two rules that follow:

- **A `Pending` provider says why.** Not implemented, no reliable API, or
  planned. A shell that quietly reads 0 for the battery is worse than one that
  says the battery provider is not live.
- **A driver limitation is documented, not worked around.** Per-pixel alpha
  being unavailable is in [WINDOWS_API.md](WINDOWS_API.md) with the exact reason
  and the exact machine it happened on.

### Do not add a timer

The frame loop has no timer, and the idle budget depends on that. Before adding
anything that samples on a schedule, read [PERFORMANCE.md](PERFORMANCE.md)
§1, and look at `battery` and `system.cpuUsage` for what "not implemented"
looks like when there is no event to subscribe to.

### An error is a diagnostic, not a panic

A config must never crash the shell. A bad element becomes a visible
placeholder; a failed build keeps the previous scene. Three levels, described in
[ARCHITECTURE.md](ARCHITECTURE.md) § Error boundaries.

### `did you mean` when something is genuinely close

The edit-distance budget is deliberately tight
(`diag::closest`, `(len / 3).max(2) + 1`). A confidently wrong suggestion is
worse than none, so a name with nothing near it gets no suggestion at all.

### Comments explain why, not what

`// increment i` is noise. `// The user data is cleared first, which is what tells
the window procedure this teardown was ours and not the user's close` is a
comment. Most of the non-obvious decisions in this codebase have one, and they
are the reason a future reader can tell a constraint from an accident.

---

## Testing

327 tests, and they run in under a second.

```
cargo test --workspace              # everything
cargo test -p slowshell-core        # the language, no display needed
cargo run -p slowshell --example render_probe   # the renderer, by pixels
```

What is covered, and what each level is for:

| Level | What it proves |
|---|---|
| `slowshell-core` | The language: parsing, evaluation, reactivity, diagnostics, the registry, the IPC framing. No display needed. |
| `slowshell-runtime` | The pipeline: loading, compiling, providers, handler validation, and that every config in `examples/` still builds. |
| `slowshell-ui` | Layout and style, with rectangles asserted as numbers. |
| `slowshell-win` | Argument quoting, colour byte order, hotkey parsing, clipboard round-trips, endpoint liveness. |
| `render_probe` | The actual pixels: fills, strokes, text, and every font weight. |

### When adding a test

- **Name it for the failure it prevents**, not for the function it calls.
  `a_broken_element_becomes_a_visible_placeholder` tells you what broke;
  `test_element_3` does not.
- **Assert the specific thing.** A test that passes for the wrong reason is
  worse than no test, because it will still be there when the real thing breaks.
- **Prefer a real call over a mock.** The clipboard test round-trips real text
  through the real clipboard, guarded by a mutex, because the thing that went
  wrong once was a race that a mock would not have had.

### Before sending a change

```
cargo test --workspace
cargo build --workspace          # zero warnings
Shell.exe --check examples\topbar.config
Shell.exe examples\topbar.config # and look at it
```

A change that adds a warning will not be merged. Warnings are the shell telling
you it does not understand its own code, and there are not many of them to begin
with.

---

## Style

- `rustfmt` defaults. Where the default fights readability, leave a comment
  saying so.
- Comments and doc comments in full sentences, with a full stop. This codebase's
  comments are unusually long on purpose: the reasoning is the value, and a
  half-sentence loses it.
- Prefer a named constant to a magic number, especially in the renderer, where
  6.0 might be a radius or a conversion factor.
- Errors are `String` at the boundary and typed where it helps. A user-facing
  message says what to do next: *"the clipboard is in use by another program; try
  again"*, not `ERROR_ACCESS_DENIED`.

---

## Verifying a Windows API

Do not guess at a signature. The generated source is on disk:

```
%USERPROFILE%\.cargo\registry\src\index.crates.io-*\windows-0.62.2\src\
```

```powershell
Select-String -Path "…\Win32\System\Pipes\mod.rs" -Pattern "pub unsafe fn CreateNamedPipeW"
```

`windows` 0.62 has some sharp edges, all of which were hit at least once:

| | |
|---|---|
| Enums | Newtype structs with associated consts, not C-like enums |
| `FindFamilyName`'s index | Must not be null; faults on the system font driver |
| `ID2D1ImageBitmap` | Now `ID2D1Bitmap1` |
| `IDXGIDXGISwapChain1` | Now `IDXGISwapChain1` |
| `DrawText` | The 6-arg overload, with a `DWRITE_MEASURING_MODE` |
| `DXGI_ADAPTER_DESC1` | Has **no** `DriverVersion` field |
| `HANDLE` ↔ `HGLOBAL` | Not castable with `as`; both wrap the same pointer |
| `keybd_event` | Takes `u8`, and flags are a newtype |
| `LockWorkStation` | In `System::Shutdown`, not `System::UserAccess` |
| `ConnectNamedPipe` | Gated behind the `Win32_System_IO` feature |

A feature-gated function that will not resolve is usually a missing feature in
`crates/slowshell-win/Cargo.toml`, not a wrong path.

---

## Performance changes

Performance claims need a measurement, in the commit message, in the same form
as [PERFORMANCE.md](PERFORMANCE.md) uses:

```
idle CPU 0.01% -> 0.008% of one core, 1920x1080, one bar, debug build
```

Not "faster". Not "much less CPU". A number, the configuration it was taken
under, and the build profile.

The most useful performance commits so far were the ones that *removed* work:
draining the reactor's `pending` queue, sharing `display_text()` between layout
and paint, and returning early from `publish_system` on an unchanged value.

---

## Reporting a bug

The two most useful things in a bug report:

1. **`Shell.exe --doctor` output.** It names the adapter, the presentation path,
   the alpha mode, the displays, and every provider with its state.
2. **The exact config**, ideally as a `--check` run, so the diagnostic is in the
   report rather than described.

```
Shell.exe --check your.config
Shell.exe your.config     # with $env:SLOWSHELL_LOG="debug"
```

A pixel-level description of what was drawn is worth more than a description of
what was expected. `debug` logging prints the resolved layout tree and the
per-frame draw statistics.

---

## Licence

MIT. See [LICENSE](LICENSE).
