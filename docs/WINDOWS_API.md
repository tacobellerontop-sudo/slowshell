# Windows API

Which Windows API backs what, and — more usefully — where there isn't one.

Slowshell's rule is that a feature is either built on a documented Windows API,
or it is reported as missing. Nothing is built on a fragile assumption that
happens to work on one machine.

---

## Windowing

| | |
|---|---|
| Windows and messages | `CreateWindowExW`, a `WNDCLASSW`, and a window procedure. `crates/slowshell-win/src/window.rs` |
| Top-level, no caption, no taskbar entry | `WS_EX_TOOLWINDOW \| WS_EX_TOPMOST`, `WS_POPUP` |
| Per-pixel click-through | `WM_NCHITTEST` returning `HTTRANSPARENT` over regions with no handler |
| Borderless resize | `WM_NCCALCSIZE` returning `0` |
| Taskbar exclusion | `WS_EX_TOOLWINDOW` — a tool window has no taskbar button |
| DPI awareness | `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` |
| Per-monitor DPI | `WM_DPICHANGED`, with the suggested rectangle taken from `wParam` |
| Message pump | `PeekMessageW` / `TranslateMessage` / `DispatchMessageW`, non-blocking, drained once per frame |

### Why the message pump is non-blocking

`GetMessageW` blocks until a message arrives, which is what a normal application
does. A shell cannot: it has work of its own on a schedule, and a blocking pump
would mean the clock, the file watcher and `shellctl` all only ran when someone
moved the mouse.

So the loop uses `PeekMessageW` and sleeps until the next thing that could
matter. That is why idle CPU is ~0: the shell is asleep, not spinning, and it
knows the exact moment the displayed clock could change.

### `WM_DESTROY` and the user-data check

`window::destroy` clears `GWLP_USERDATA` *before* `DestroyWindow`. The window
procedure then treats a `WM_DESTROY` with no user data as the shell's own
teardown rather than as the user closing the window.

Without that, `WM_DESTROY` is indistinguishable from a close request, and the
shell quits the first time a config reload removes a panel — which is the most
ordinary thing a shell does.

### Exclusive zones

A `Panel` with `exclusive: true` reserves screen space, so maximised windows stop
at the bar instead of covering it. This is the app bar protocol:
`SHAppBarMessage(ABM_NEW)` to register, `ABM_SETPOS` to reserve a rectangle, and
`ABM_REMOVE` on shutdown. Windows keeps the list, sums the strips, and shrinks
every application's work area by the total. It is the same mechanism Explorer's
own taskbar uses, which is why a Slowshell bar and the real taskbar coexist
instead of fighting.

| | |
|---|---|
| Registration | `SHAppBarMessage(ABM_NEW)`, then `ABM_ACTIVATE` |
| Reservation | `SHAppBarMessage(ABM_SETPOS)` |
| Release | `SHAppBarMessage(ABM_REMOVE)`, in `Drop` |
| Ownership | The window holds its `AppBar`, so `window::destroy` releases it |
| Re-negotiation | `WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE`, resize, DPI change |

Three things about this are worth knowing, because each was found by measurement
rather than by reading the documentation:

**The position comes from the work area, not the screen edge.** `ABM_SETPOS` needs
an explicit rectangle, and the obvious one — the display's own edge — puts a
bottom bar straight through Explorer's taskbar when they share an edge. The work
area, from `GetMonitorInfoW`, already excludes every other app bar, so anchoring
there is what makes bars stack. Two Slowshell bars on one edge stack too, because
the monitor list is re-read as each one is placed.

**Ownership is what makes it safe.** `AppBar::drop` sends `ABM_REMOVE`, and the
window owns its `AppBar`. A shell killed mid-frame gives its space back; a value
written to the registry would not. This is why the registry route was removed
rather than left as a fallback.

**The callback message id cannot be trusted.** `ABM_NEW` returns a message id to
handle in the window procedure, and on some systems — including the machine this
was developed on — it returns **1**, which is `WM_CREATE`. Matching a window
procedure on that swallows every window's `WM_CREATE` and fires a zero-rectangle
position request at creation, which makes the *real* request that follows be
ignored. Every call then reports success and nothing is reserved. So the id is
validated against `WM_USER` before it is ever matched on, and an implausible one
is discarded: the space is still reserved, only the overlap-avoidance handshake
is skipped. Losing the handshake is survivable; corrupting the registration is
not.

There is no auto-hide. `ABM_GETAUTOHIDEBAREX` is read, but nothing here slides
the window, because only the owner of a window can. `shellctl screens` says so
when the OS reports that edge as auto-hidden, since "my bar does not slide away"
otherwise looks like a bug rather than an unimplemented feature.

`shellctl screens` reports the work area per monitor and what this shell has
reserved. The work area is the authoritative answer, re-read on every call;
`ABM_GETTASKBARPOS` is only a lower bound, because Windows does not reliably
disclose other processes' bars — Explorer's taskbar in particular may not appear
at all.

To check the mechanism on a new machine, `cargo run -p slowshell-win --example
appbar_probe` walks the candidate window styles and reports which ones Windows
accepts.

### What Windows will not let a shell do

Two things a Linux shell takes for granted have no supported Windows route. Both
were established by probing the OS rather than by reading documentation, with
`cargo run -p slowshell-win --example capability_probe`.

**Virtual desktops cannot be listed or switched.** `IVirtualDesktopManager`
(`CLSID_VirtualDesktopManager`, `aa509086-5ca9-4c25-8f95-589d3c07b48a`) is
documented, instantiates fine, and has three methods — of which only
`MoveWindowToDesktop` does anything a workspace indicator needs. Enumerating and
switching live on `IVirtualDesktopManagerInternal`, which is not in the SDK at
all and returns `E_NOINTERFACE` on this machine.

So `virtualDesktops.count` and `virtualDesktops.current` are seeded and never
published. They exist only so a config that reads them still compiles. **Do not
build a workspace indicator on them**: a row of numbers that always reads `1`
looks like a broken workspace bar, which is worse than not having one. To get
real workspace switching, bind `onClick` to whatever third-party tool you use —
the shell cannot switch desktops, so something else has to.

**Now playing is reachable, contrary to what this crate used to claim.** SMTC has
two doors. The per-application `SystemMediaTransportControls.GetForCurrentView`
requires a package identity, which a desktop process does not have. The
**global** `GlobalSystemMediaTransportControlsSessionManager` does not, and it
lists every process's current session. The probe on this machine:

```
now playing
  SMTC global manager              RequestAsync OK
    GetCurrentSession      no current session
    sessions               0
```

An earlier version of `examples/media.config` said this was impossible. It was
true of the wrong door, and the file said so; that has been corrected.

The cost model is why the watcher is a thread rather than a subscription. SMTC's
`SessionsChanged` and `CurrentSessionChanged` events arrive through a COM message
pump on the thread that requested them, and the frame loop must never block on a
COM call. So `slowshell_win::media` owns a background thread that asks on an
interval, and the frame loop reads a snapshot — one mutex read per tick, and a
repaint only when the snapshot actually changed. Async operations are *polled*,
not awaited, because a shell has no async runtime and `Status()`/`GetResults()` is
all a background thread needs.

The active window title is read with `GetForegroundWindow` +
`GetWindowTextW`, once a second alongside the clock, never per frame. A window
whose pid is the shell's own reports empty, so opening the launcher does not
rename the bar to "launcher".

---

## Displays and DPI

| | |
|---|---|
| Enumeration | `EnumDisplayMonitors` + `GetMonitorInfoW` |
| Scale | `GetDpiForMonitor` (`MONITOR_DPI_TYPE_PER_MONITOR_DPI`) |
| Refresh rate | `EnumDisplaySettingsW` with `ENUM_CURRENT_SETTINGS` |
| Source rectangle | `EnumDisplaySettingsW` with `DEVMODE` |
| Rotation | `EnumDisplaySettingsW` `DisplayOrientation` |
| Work area | `SystemParametersInfoW(SPI_GETWORKAREA)`, per monitor |
| DPI change | `WM_DPICHANGED` |

### Logical pixels

Layout works in **logical pixels**; Win32 reports physical. The conversion uses
the monitor's *scale*, not its DPI, and the two are different numbers — mixing
them up was a real bug that made every bar the wrong size at 150%.

`SurfaceState::logical_size` divides by the monitor's scale, and
`surface_dpi` multiplies the base DPI by it, so the two never disagree.

A bar therefore keeps the same apparent size across a mixed-DPI setup, which is
the entire point of per-monitor v2.

---

## Rendering

| | |
|---|---|
| Device | `ID3D11Device` via DXGI, `D3D11_CREATE_DEVICE_BGRAP_SUPPORT` |
| Context | `ID2D1DeviceContext2` |
| Swap chain | `IDXGISwapChain1`, flip model |
| Text | `IDWriteFactory5`, `IDWriteTextFormat` |
| Brushes | `CreateSolidColorBrush`, cached per colour |

### Two presentation paths

**Flip-model swap chain** (`Presentation::Dxgi`) — preferred. Independent
presentation, per-pixel alpha, DWM backdrops work.

**`ID2D1HwndRenderTarget`** (`Presentation::Hwnd`) — the fallback. Presents
opaquely.

One drawing API, `RenderTarget` in `render/target.rs`, with a blanket impl over
anything that can be a Direct2D target. The `Painter` is written against the
trait, so it does not know which path it is on.

### The probe, and its cache

Choosing between the two is a runtime property, not a build-time one, so
`Graphics::create` probes: create a swap chain, try to draw into it, and check
it really took.

That probe costs **3.2 seconds** on the development machine. A shell that spends
three seconds deciding how to start has already failed its startup budget, so
the verdict is cached on disk at `%LOCALAPPDATA%\Slowshell\presentation.json`,
keyed on the adapter's LUID and PCI ids. A driver update changes the key.

```
SLOWSHELL_REPROBE=1        force a fresh probe
```

### Device loss

`D2DERR_RECREATE_TARGET` means the device was lost. The shell rebuilds the
window target and carries on rather than exiting.

### Why per-pixel alpha is sometimes unavailable

This is a driver limitation, not a design choice, and it is worth being precise
about.

Direct2D requires **premultiplied** alpha. A flip-model swap chain created with
`DXGI_ALPHA_MODE_PREMULTIPLIED` or `DXGI_ALPHA_MODE_STRAIGHT` supports it. The
AMD Radeon 780M under Hyper-V — the development machine — rejects every
alpha-aware swap chain, and then rejects `CreateBitmapFromDxgiSurface` for the
opaque one, because an `DXGI_ALPHA_MODE_IGNORE` surface has no alpha for D2D to
premultiply.

The shell therefore falls back to `ID2D1HwndRenderTarget`, which presents
opaquely. On that machine:

```
$ Shell.exe --doctor
  presentation window render target (opaque)
  alpha        discarded at present; translucent colours are pre-composited
```

Translucent colours are **pre-composited** against the panel's own background, so
a `80%` opaque panel looks like an `80%` opaque panel rather than a solid one
with the wrong colour. On hardware that accepts an alpha-aware swap chain, the
flip-model path is used and panels are genuinely translucent.

There is no workaround on this machine, and inventing one would be worse than
saying so.

---

## Typography

| | |
|---|---|
| Factory | `IDWriteFactory5` |
| Family lookup | `IDWriteFontCollection::FindFamilyName` |
| Format | `IDWriteTextFormat`, cached per (family, size, weight, alignment) |
| Layout | `IDWriteTextLayout`, cached per (text, format) |
| Metrics | `IDWriteTextLayout::GetMetrics` |

### DIPs versus pixels

DirectWrite works in **DIPs** — 1/96 inch — and layout works in logical pixels.
Getting this wrong scales every glyph twice at 125%, which is a real bug that
was found by measuring a rendered string's width, not by reading the code.

The rule: DirectWrite metrics come back in DIPs and are divided by the *monitor
scale* exactly once. `render/text.rs` is the only place that converts, and
nothing else may.

### `FindFamilyName` and its index out-parameter

`IDWriteFontCollection::FindFamilyName` requires a non-null index out-parameter,
even to say "not found". Passing null faults on the system font driver. It is
declared as `Option` in the `windows` crate, so it is easy to get wrong; the
call site passes a real one.

### Font weights

Every weight from 100 to 900 is supported, and `render_probe.rs` asserts each
one renders differently. A weight silently falling back to 400 is the kind of
thing nobody notices until every label looks the same.

---

## Colour

| | |
|---|---|
| Format | `D2D1_COLOR_F` premultiplied |
| sRGB | `D2D1_COLOR_SPACE_SRGB` |
| Brush cache | One brush per distinct colour, per target |
| Alpha flatten | Manual composite when the target presents opaquely |

Translucent colours are pre-multiplied on the CPU when the presentation path
discards alpha, and used as-is when it does not. The painter does not need to
know: `Surface` carries the mode and the compositing is a single branch.

---

## Input

| | |
|---| |
| Pointer | `WM_MOUSEMOVE`, `WM_LBUTTONDOWN`, `WM_MOUSEWHEEL` |
| Keyboard | `WM_KEYDOWN`, `WM_KEYUP`, `WM_CHAR` |
| Hotkeys | `RegisterHotKey` |
| Injection | `keybd_event` |
| Modifiers | `GetKeyState` |

Input is **injected** rather than posted where a key must reach the system, not
just this process. A posted `WM_KEYDOWN` is ignored by anything that reads the
keyboard state directly, and both `Win+D` and the media keys do.

A key with no sendable virtual-key code is refused rather than guessed.

### Click-through

`WM_NCHITTEST` returns `HTTRANSPARENT` wherever no widget has an `onClick`. The
result is that a bar with no handlers is completely click-through, which is
correct — and also means **the default config has zero clickable regions**,
because it has no handlers. That is a real limitation of the default
configuration, not of the mechanism, and adding one `onClick` makes the region
publish.

---

## The clipboard

| | |
|---|---|
| Open | `OpenClipboard` |
| Read | `GetClipboardData(CF_UNICODETEXT)` + `GlobalLock` |
| Write | `GlobalAlloc(GMEM_MOVEABLE)` + `SetClipboardData` |
| OLE | `OleInitialize` on the accessing thread |

`CF_UNICODETEXT` is the only format handled, because it is the only one a shell
needs. A clipboard holding a bitmap is not an error: `clipboard.get()` returns
`null` and a widget shows its placeholder.

`OpenClipboard` retries for about 100 ms. Windows allows one process to hold the
clipboard open, and plenty of things do — a file manager copying a file, another
thread in the shell. Giving up on the first refusal turns a momentary overlap
into a lost copy.

Reading walks to the null terminator with a 1 MiB bound, because a clipboard
written by another process can be truncated mid-string and walking off the end of
the allocation is worse than returning a short string.

---

## Processes

| | |
|---|---|
| Open | `ShellExecuteW` |
| Run and wait | `CreateProcessW` with `CREATE_NO_WINDOW` + `WaitForSingleObject` |
| Exit code | `GetExitCodeProcess` |

`CreateProcessW` gets the application name and the command line as separate
parameters, so a program path with spaces in it does not have to survive being
re-parsed out of a command line.

Arguments are quoted with the documented Win32 rule: a backslash run before a
quote is doubled, and a backslash run before the closing quote is doubled again.
A naive quote produces a command line where the rest of a filename is read as a
new argument, which turns one program into a different one. Both cases are
tested.

---

## IPC

| | |
|---|---|
| Transport | Named pipe, byte mode |
| Server | `CreateNamedPipeW` with `PIPE_UNLIMITED_INSTANCES` and `PIPE_REJECT_REMOTE_CLIENTS` |
| Client | `CreateFileW` + `WaitNamedPipeW` |
| Discovery | A small endpoint file, with a liveness check |

Named pipes rather than a TCP socket, because a socket needs a port and a port is
something anything on the machine can connect to. A pipe's name lives in the
kernel object namespace: the same user reaches it and nobody else does.

Three details, each of which cost real debugging:

**The server runs on its own thread.** `ConnectNamedPipe` blocks until a client
arrives. On the UI thread that would freeze the bar for as long as nobody runs
`shellctl`, which is essentially always.

**The pipe name carries the pid.** Two shells on one desktop must not share a
pipe and steal each other's requests. But a `shellctl` does not know the shell's
pid, so the shell writes its endpoint to `%LOCALAPPDATA%\Slowshell\control.json`
and the CLI reads it. That solves the crash case for free: a file left by a
killed shell names a pid that is no longer running, which is exactly the check
needed to say "no slowshell is running" in a millisecond instead of hanging.

**The next pipe instance is created before the current one is closed.** Otherwise
there is a window in which the name has no live instance, the client's
`CreateFileW` succeeds, and its read fails with `ERROR_BROKEN_PIPE` — which reads
as "the shell crashed" when it did not.

---

## DWM

| | |
|---|---|
| Backdrop | `DwmSetWindowAttribute` with `DWMWA_SYSTEMBACKDROP_TYPE` |
| Dark mode | `DWMWA_USE_IMMERSIVE_DARK_MODE` |
| Corner preference | `DWMWA_WINDOW_CORNER_PREFERENCE` |
| Supported | `DwmIsCompositionEnabled` |

Backdrops need Windows 11. `shellctl doctor` reports whether they are available.
A backdrop is only applied on a path that presents per-pixel alpha — compositing
a blur under a solid background would draw it over the bar and look like a
rendering fault.

---

## What is not implemented, and why

Every entry here reads as `null` in a config. `shellctl doctor` prints this list
with each provider's current state.

| Provider | Why |
|---|---|
| `battery` | `GetSystemPowerStatus` works but is undocumented and returns nothing useful on a desktop. `CallNtPowerInformation` is the supported route but needs a battery-class device, which a desktop does not have. |
| `network` | `GetIfTable2` gives bytes and needs polling to become a rate. Wi-Fi SSID needs `WlanQueryInterface`, and only works for the interface Windows has a profile for. |
| `audio` | `IAudioEndpointVolume` is a COM interface in `mmdeviceapi.dll` that the `windows` crate does not bind. Reading volume also needs an endpoint enumeration, which is a timer. Actions work; reading does not. |
| `system` | CPU needs `PDH` counters with a collection interval — a timer. Memory is `GlobalMemoryStatusEx` and would work. |
| `windows` | The foreground window's title and process need `GetForegroundWindow` plus `GetWindowText`, which for other processes' windows is a cross-process read that is often blocked, and the active-window *list* has no public API at all. |
| `virtualDesktops` | Virtual desktops are undocumented. There is no public API, and the ones that exist are COM interfaces into `vdw10.dll` with no header. |
| `media` | The currently playing track is exposed only through Global System Media Control Transport, which needs a UWP app identity or a proxy service. |
| `tray` | `Shell_NotifyIcon` works, but a shell that owns the notification area has to implement the full icon contract, including the callbacks for double-click and context menu. |

Two of these — `memoryUsage` and the foreground window — are within a day's work
each. `virtualDesktops` and `media` are not, and pretending otherwise would mean
either a private API or a service, and Slowshell does not do either.
