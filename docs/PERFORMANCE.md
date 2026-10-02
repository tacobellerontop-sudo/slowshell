# Performance

The budgets, and the mechanisms that hold them.

---

## The budgets

| | Budget | Measured |
|---|---|---|
| Idle CPU, bar with a 1-second clock | < 2% of one core | **0.2–1.2%**, see below |
| Idle CPU, bar with no clock | < 2% of one core | **0.1%** |
| Working set | < 100 MB | **40 MB** |
| Startup to first pixels | < 1 s | **~290 ms** cold, ~75 ms warm |
| Frame cost, 20-element bar | < 4 ms | **~0.2 ms** |
| `shellctl` round trip | instant | **~17 ms** |
| Binaries | small | `Shell.exe` 1.2 MB, `shellctl.exe` 0.9 MB |
| `exclusive: true` adds to idle | none measurable | **0%**, see below |

Measured on the development machine: AMD Radeon 780M under Hyper-V, 1920×1080,
one visible top bar, **release** build, averaged over 30 seconds of idle.

```
cargo build --release
target\release\Shell.exe examples\topbar.config
# elsewhere
$p = Get-Process Shell; Start-Sleep 5
$b = $p.CPU; Start-Sleep 30
[math]::Round((($p.CPU - $b) / 30) * 100, 3)     # percent of one core
```

**Read the idle figure as a range, not a point.** The same config measured on
this machine has returned 0.19%, 0.39%, 0.63%, 0.81%, 1.25% and 0.86% across six
identical runs, and a config doing no work at all swung from 0.08% to 0.23%. A
Hyper-V guest shares a core with the host, so scheduler noise swamps a quantity
this small. Take the minimum of several runs as the least-contaminated estimate,
and do not treat a single reading as a measurement. The budget — under 2% — is met
by a wide margin in every run.

`exclusive: true` costs nothing at idle, and that is structural rather than lucky:
the window re-reserves its strip only on `WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE`,
a resize, or a DPI change — never per frame — and re-reserving the same
rectangle is a no-op that sends no work-area change. Measured by running two
otherwise-identical configs, four runs each, 25 seconds per run: the minimum with
`exclusive` was *lower* than without it. That is what "no measurable cost" looks
like when the noise is larger than the signal.

Animations cost nothing when they are not running, which is the only acceptable
answer for a shell that claims an idle budget. The frame loop asks
`step_reveals` whether anything is moving; a settled reveal answers with one branch
per surface and the loop returns to blocking in the kernel immediately. Measured
with `examples\caelestia.config`, which has three reveal panels: idle minimum
**0.25%**, and zero resize events logged during the measurement window — which is
the direct evidence that a settled panel is not holding the loop awake, since a
held-awake loop would resize and repaint continuously.

`now playing` is the one subsystem that polls. SMTC's `SessionsChanged` event
arrives through a COM message pump, and the frame loop must never block on a COM
call, so a background thread asks on an interval and the frame loop reads a
snapshot. That is one mutex read per tick, and the snapshot only publishes when a
track title, artist, status or app actually changed — publishing an identical
snapshot is explicitly not a change, which is asserted in
`publishing_the_same_track_twice_is_not_a_change`.

The idle figure is not a benchmark trick. It is a structural property, and the
rest of this document is the structure.

## Where the last 0.1% goes

The frame loop blocks in the kernel until a message arrives, so on a still
desktop it wakes only for real work. Two things still cause a wake every second:

1. **The clock.** `publish_system` republishes `clock.*` once a second, because a
   clock has no event to subscribe to. A config with a `Clock` therefore wakes
   once a second; one without wakes never for this reason.
2. **The file watcher.** `poll_files` stats each watched file once per loop turn
   so a config edit is noticed. `ReadDirectoryChangesW` would make this a true
   event — and `notify` is already in `Cargo.toml` for exactly that — but a
   dependency is only worth taking on once its behaviour has been tested on the
   machines this shell is meant to run on, and it has not been.

Both are once-a-second `stat` and clock reads, which is the 0.1–0.26% above.
Every *other* system value is event-driven, and no widget is recomputed unless
the value it read changed.


---

## 1. No polling, anywhere

The frame loop has **no timer**. It sleeps until the next thing that could
matter:

```rust
let until_clock = self.next_clock.saturating_duration_since(now);
let until_clock = until_clock.clamp(Duration::from_millis(4),
                                    Duration::from_millis(200));
let until_message = Duration::from_millis(50);
std::thread::sleep(until_clock.min(until_message));
```

A polling design — "check the clock every second, check the battery every five
seconds" — costs CPU whether or not anything changed, and it scales with the
number of values rather than the number of changes.

The push model costs nothing when nothing changes. `publish_system` asks the
reactor whether anything actually did:

```rust
if self.publish_system() {   // returns true only if a value changed
    self.frame();
    continue;
}
```

`Reactor::set` returns a `bool`. A provider that pushes an unchanged value tells
the shell "nothing to do", and the shell skips the frame. The clock ticking the
same second twice costs nothing.

This is also why several providers read as `null` rather than being sampled. A
sampled `system.cpuUsage` would be easy to add and would cost a `PDH` collection
every second forever. It is not implemented, and the reason is this section.

---

## 2. A frame does zero dependency bookkeeping

A naive reactive system walks its graph every frame, checks what is dirty, and
invalidates what it read. That bookkeeping *is* the cost, and it scales with the
size of the config whether or not anything changed.

Slowshell inverts it. Dependencies are recorded **once, at compile time**:

```rust
// Building the node inside `tracked` records the properties it reads.
let _ = tracked(|| eval(&env, &expr)).0;
let id = reactor.derived(path, move || eval(&env, &expr));
```

The frame loop then only *pulls*:

```rust
// resolve_values, every frame
reactor.get(node)   // cached unless pending
```

A node that is not pending is a pointer read. A widget that reads no system state
is never recomputed at all, no matter how many widgets exist.

### The tracker is a stack

A single dependency slot is enough if a binding reads one value. An expression
like `battery.percentage + " of " + network.ssid` reads two, and both edges must
be recorded or the widget silently stops updating when the *second* one changes.

That was a real bug, found by a test that renders and reads pixels rather than
one that inspects the graph.

---

## 3. One render path, two backends

`RenderTarget` is a trait with a blanket impl. The `Painter` is written against
it, so the flip-model swap chain and the `ID2D1HwndRenderTarget` fallback cost
the same code to maintain and the same code to run.

The probe that picks between them costs **3.2 seconds** on the development
machine, which is why its verdict is cached on disk, keyed on the adapter's LUID
and PCI ids. A driver update changes the key.

```
SLOWSHELL_REPROBE=1        force a fresh probe
```

A shell that spent three seconds deciding how to start would have failed the
startup budget before the bar appeared.

---

## 4. Hot reload does not recreate windows

`build()` replaces the element trees and keeps the windows. A window is recreated
only when the panel count changes or a panel's declared name changes.

So editing a colour in a config does not make the bar blink, which is the whole
point of hot reload. A build that *fails* keeps the previous scene entirely, so a
half-typed line never blanks the desktop.

`poll_files` stats each watched file and compares modification times, so a touch
that changes nothing costs one `stat` and nothing else.

---

## 5. Layout and paint are only run when something changed

`SurfaceState` carries `needs_layout`. A frame re-resolves every reactive value —
that is cheap, it is a pointer read per node — and compares the result to the
previous one. Layout runs only if a resolved value actually changed.

`Resolved` derives `PartialEq` for exactly this. Without it, a changing clock
would re-lay-out the bar 60 times a second, because the *time* changed, even
though the *width* of every string did not.

Text measurement is the expensive part of layout, and it is cached in two places:

- `TextMeasurer` wires real DirectWrite measurement into the layout pass, with
  formats and layouts cached by (family, size, weight) and by string.
- `Element::display_text()` is shared by layout and paint, so a string is
  computed once. They used to disagree, and the result was a bar that laid out
  with one string and drew another.

---

## 6. Brush and format caches

Brushing is a COM allocation. One brush per distinct colour per target, cached,
means a bar of twenty elements in four colours makes four brush calls per frame
rather than twenty.

`IDWriteTextFormat` is cached by (family, size, weight, alignment) and
`IDWriteTextLayout` by (text, format). Both are keyed on the surface DPI, so a
window that moves between monitors at different scales gets correct metrics
rather than a cached wrong answer.

---

## 7. The IPC server is off the UI thread

`ConnectNamedPipe` blocks until a client arrives. On the UI thread that would
freeze the bar for as long as nobody runs `shellctl`, which is essentially
always — so the whole idle budget would be spent blocked in a syscall.

One thread accepts connections and hands requests over a channel. The frame loop
drains it once per turn, in the same place it already drains window messages.
The thread costs nothing while no client is connected, because it is blocked in
the kernel.

---

## 8. Idle is blocked, not spinning

`PeekMessageW` is non-blocking, and `GetMessageW` would block — which a normal
application wants and a shell cannot, because the clock, the file watcher and
`shellctl` all have to run on a schedule.

So the loop drains what is queued, and then, if nothing changed, calls
`MsgWaitForMultipleObjectsEx`, which blocks **in the kernel** until a message
actually arrives or the timeout expires. A still desktop is asleep in the
kernel, not spinning in user mode.

This replaced a `sleep` with a 50 ms ceiling, which existed only so a `shellctl`
request would be answered within a frame or two. That ceiling was the entire
idle cost: twenty wake-ups a second, each one a message pump and a few stats.

The fix was not a longer sleep — it was making the IPC thread *post a message*
when a request lands, so the wait can be as long as the shell genuinely has
nothing to do and a `shellctl` still arrives instantly:

```
IPC thread: accept → send request to channel → PostMessage(WM_SLOWSHELL_WAKE)
                                                        ↓
frame loop: blocked in MsgWaitForMultipleObjectsEx ── wakes ─→ answer
```

Without the posted message, a `shellctl` on a config with no `Clock` in it would
wait for ever.


---

## Where the remaining time goes

On a live frame, roughly:

| | |
|---|---|
| Resolve reactive values | pointer reads per node |
| Layout, if something changed | DirectWrite measurement, mostly cached |
| Paint | a few dozen Direct2D calls |
| Present | one flip |

The frame loop reports what it drew, and warns on an empty one:

```
WARN  surface 0 drew nothing this frame
WARN  surface 0 dropped 3 of 40 text runs
```

That is how ten of the bugs found during development were found — by looking at
what came out, not by reading what went in.

---

## Adding a feature: the checklist

Before adding a system value or a widget that could tempt a timer:

1. **Can it be pushed?** If a Windows event exists, subscribe to it. If it must be
   sampled, ask whether the sample rate can be as low as a minute rather than a
   second — and whether the value is worth the CPU at all.
2. **Is it read?** A value no config reads costs one `set` and nothing else. That
   is cheap, and it is the right way to land a provider before its consumers.
3. **Does it change the layout?** If yes, it costs a layout pass. Not fatal, but
   it belongs in the frame budget.
4. **Does it need polling?** If it does, the answer is usually no. `audio.volume`
   is a worked example: the *actions* work, the *read* does not, and the reason
   is this document.

---

## Profiling

The log is the profiler.

```
$env:SLOWSHELL_LOG="debug"
Shell.exe examples\topbar.config
```

`debug` prints the resolved layout tree and the per-frame draw statistics,
which together answer "why is this widget in the wrong place" and "why is
nothing being drawn" without a debugger.

```
shellctl graph          node, source, derived and edge counts
shellctl status         uptime and panel count
```
