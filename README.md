# Slowshell

A programmable, reactive desktop shell for Windows 11.

Written directly against Win32, Direct2D and DirectWrite. No browser, no
Electron, no embedded Chromium, no runtimes. A bar and a config file.

```rust
Panel {
    position: "top"
    background: "#11111bcc"
    padding: 0

    left  { Row { gap: 10  padding: [0, 12]  Text { text: "Apps"  fontWeight: 600 } } }
    center { Row { Clock { format: "HH:mm" } } }
    right { Row { gap: 14  padding: [0, 12]  Text { text: "WiFi"  color: "foregroundMuted" } } }
}
```

That is a real, running shell. It is about twenty lines, and the first run
writes it for you.

---

## Running it

```powershell
git clone https://github.com/slowshell/slowshell
cd slowshell

cargo shell          # run the shell
```

`cargo shell` **does not return** - a desktop shell runs until you stop it. That
is not a hang; open a second terminal and talk to it:

```powershell
cargo ctl -- status        # what is running
cargo ctl -- screens       # displays, work areas, and what the shell reserves
cargo ctl -- reload        # re-read the config
cargo ctl -- open launcher # show a named surface
cargo ctl -- stop          # quit the shell
```

To try one of the examples instead of the starter config, pass it a path:

```powershell
cargo shell -- examples\exclusive.config   # reserves screen space
cargo shell -- examples\topbar.config
```

### Does it hold screen space?

The starter config's panel has `exclusive: true`, so the bar's 40px comes out of
the usable desktop and maximized windows stop below it instead of covering it.
Confirm it:

```powershell
cargo ctl -- screens
```

```
primary   1920x1080  @1.0  origin 0,0  60Hz  primary
          work area 0,40 1920x992
reserved by slowshell
  panel          top          0,0     1920x40
```

The work area is the number that matters: it is what Windows tells every other
application they may use. Compare it with `cargo shell -- --doctor`, which prints
the same figure with nothing of Slowshell running.

Delete the `exclusive: true` line for an overlay bar that windows do cover, or
change `position` to `"bottom"` and the bar reserves the bottom edge instead,
stacking above the real taskbar rather than through it.

### Seeing the current defaults

The starter config is written once, on first run, and never rewritten — your file
is yours. So when the defaults improve, your existing file does not change with
them. To see what the current defaults are:

```powershell
cargo run -p slowshell -- --starter      # writes shell.starter.config and exits
```

It never touches your live config, and it refuses to overwrite anything,
including your live config, without you naming a different path.

Or run it in the background and keep your terminal:

```powershell
Start-Process .\target\release\Shell.exe
.\target\release\shellctl.exe status
```

### Why not `cargo run`?

Because this workspace has **two** binaries, and `cargo run` will not guess:

```
error: `cargo run` could not determine which binary to run.
       Use the `--bin` option to specify a binary, or the `default-run` manifest key.
available binaries: Shell, shellctl
```

The aliases above exist so you never have to remember `-p slowshell`. If you
prefer to be explicit:

| | |
|---|---|
| `cargo run -p slowshell` | the shell |
| `cargo run -p shellctl` | the control CLI |
| `cargo run -p slowshell -- examples\topbar.config` | the shell, with a config |
| `cargo validate` | compile the config, print problems, **exits** |
| `cargo pixels` | the pixel-level render test |

### If cargo says "Access is denied"

`Shell.exe` is already running, so the linker cannot replace it. Either stop it
or just talk to the one that is:

```powershell
cargo ctl -- stop
# or
Get-Process Shell | Stop-Process -Force
```

### First run

`Shell.exe` with no arguments writes a starter config to
`%APPDATA%\Slowshell\shell.config` and shows a working bar with a live clock, a
clickable `Apps` label that opens a small launcher, and a toggle for the error
overlay. Edit that file and it reloads on save.

---

## Why this exists

Every desktop shell worth using is either a closed application you configure
through a settings dialog, or a framework you learn by reading its
documentation. Slowshell is the second kind, on Windows, where nobody has built
it, and it is built so that the framework is *readable*: a config file is the
complete description of your desktop, the language is small enough to hold in
your head, and when something is wrong it tells you what and where.

Three things are not negotiable, and every decision below follows from them:

| | |
|---|---|
| **Idle costs nothing** | A still desktop must not spin. No polling, ever. |
| **A bad config never blanks the desktop** | Errors are reported, in place, and the last good shell keeps running. |
| **Native or nothing** | Windows APIs, or a documented reason there isn't one. Never a fragile hack. |

## Measured

On the development machine (AMD Radeon 780M under Hyper-V, 1920×1080, one
visible top bar, release build, averaged over 30 s of idle):

| | |
|---|---|
| Idle CPU, bar with a 1-second clock | 0.2–1.2% of one core (see [PERFORMANCE](docs/PERFORMANCE.md)) |
| Idle CPU, bar with no clock | ~0.1% of one core |
| Working set | 40 MB |
| Startup to first pixels | ~290 ms cold, ~75 ms warm |
| Frame cost | ~0.2 ms for a 20-element bar |
| `shellctl` round trip | ~17 ms |
| `exclusive: true` | costs nothing at idle |
| Tests | 337 |

The idle range is honest: a Hyper-V guest shares a core with its host, so
scheduler noise is larger than the quantity being measured. Repeated runs of one
config ranged from 0.19% to 1.25%; take the minimum of several as the estimate.
The budget — under 2% — is met by a wide margin every time.

The idle figure is not a benchmark trick. It is a structural property: the shell
pushes system values into a reactive graph and *pulls* only the ones a widget
actually reads, and when nothing changed it blocks in the kernel until something
actually happens. The residual 0.1% is a once-a-second clock read and a
`stat` per watched file — see [PERFORMANCE.md](docs/PERFORMANCE.md) § "Where the
last 0.1% goes" for exactly what it is and what would remove it.


## Install

```
git clone https://github.com/slowshell/slowshell
cd slowshell
cargo shell
```

Two binaries are produced in `target\release`:

| | |
|---|---|
| `Shell.exe` | The shell. |
| `shellctl.exe` | Control and diagnostics for a running shell. |

They are not on your `PATH`. Either use the cargo aliases above, or copy them
somewhere that is, or call them by full path.

## Try it

```
Shell.exe                                   your config
Shell.exe examples\topbar.config            a specific config
Shell.exe --check examples\topbar.config    compile and report, show nothing
Shell.exe --doctor                          what this machine supports
```

Six worked examples are in [`examples/`](examples), from one line to a launcher:

| | |
|---|---|
| `minimal.config` | One panel, one line. Proves it runs. |
| `topbar.config` | A complete bar, themed, with a hidden launcher panel. |
| `sidebar.config` | A vertical dock, `exclusive`, with a section header. |
| `exclusive.config` | Reserving screen space, so maximised windows stop at the bar. |
| `desktop.config` | The one to actually use: now playing, active window, battery, network, launcher, and `wrap`. |
| `caelestia.config` | Hover the screen edges to open menus, like Caelestia Shell. Animated, and closing them costs nothing. |
| `launcher.config` | A run dialog opened on demand, with working launch actions. |
| `dashboard.config` | A floating panel of `Progress` bars and a `Container` group. |
| `media.config` | A transport bar, showing a real track title when something is playing. |

## Control it from a terminal

```
shellctl status           what is running: pid, panels, adapter, uptime
shellctl reload           re-read the config
shellctl open launcher    show a named surface
shellctl diagnostics      the last build's problems, with file and line
shellctl screens          the displays, with scale and refresh rate
shellctl graph            the size of the reactive graph
shellctl logs --level warn
shellctl doctor           the environment, no shell needed
shellctl actions          everything a config may call
```

`shellctl` talks to the shell over a per-user named pipe, and finds it through
a small endpoint file whose liveness it checks — so a `shellctl` after a crash
says *"no slowshell is running"* in a millisecond instead of hanging on a pipe
nobody will answer.

## Configuration

The whole language, in [docs/CONFIGURATION.md](docs/CONFIGURATION.md). The
short version:

```rust
Theme { accent: "#7c9cff"  background: "#11111bcc" }   // a palette

Panel {
    position: "top"          // top | bottom | left | right | floating
    exclusive: true          // reserve space; maximised windows stop at the bar
    name: "launcher"
    hidden: true             // compiled, but no window until opened

    left   { /* anchored to the start  */ }
    center { /* centred              */ }
    right  { /* anchored to the end    */ }
}
```

Elements are `Panel`, `Row`, `Column`, `Container`, `Spacer`, `Text`, `Clock`
and `Progress`. Properties are documented per element in
[docs/WIDGETS.md](docs/WIDGETS.md).

Anything that is not a literal is an expression, and every expression that reads
system state is tracked, so this is live:

```rust
Text { text: battery.percentage + "%"  visible: battery.present }
```

### Errors tell you what to do

```
$ Shell.exe --check broken.config

Unknown Property

broken.config:14:22

Unknown property "foregorund" on Text

  did you mean: foreground
```

The same machinery catches unknown element types, misspelled theme colours,
invalid clock formats, `onClick` that names an action nobody registered, and a
`Theme` name it tries to parse as a colour. Then the last good shell keeps
running.

## Documentation

| | |
|---|---|
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | How it works, and why each crate is where it is |
| [CONFIGURATION.md](docs/CONFIGURATION.md) | The language, in full |
| [WIDGETS.md](docs/WIDGETS.md) | Every element and every property |
| [THEMES.md](docs/THEMES.md) | The palette, and how to write one |
| [ACTIONS.md](docs/ACTIONS.md) | Everything a config may call |
| [WINDOWS_API.md](docs/WINDOWS_API.md) | Which Windows API backs what, and where the gaps are |
| [PLUGIN_API.md](docs/PLUGIN_API.md) | The extension surface, and its honest limits |
| [PERFORMANCE.md](docs/PERFORMANCE.md) | The budgets and how they are held |
| [CONTRIBUTING.md](docs/CONTRIBUTING.md) | Building, testing, the house rules |

## What is not finished

Slowshell is early. It runs, and the bar is real, and the framework is sound.
These parts are not done, and the shell says so rather than pretending:

- **System providers.** `clock`, `screens` and `notifications` are live.
  `battery`, `network`, `audio`, `system`, `windows` and `virtualDesktops` read
  as `null`. `shellctl doctor` names each one and why.
- **Per-pixel alpha.** Where the GPU driver accepts an alpha-aware swap chain,
  panels are translucent and DWM backdrops work. Some drivers — including the one
  in Hyper-V on the development machine — reject it, and the shell falls back to
  an opaque window render target and says so on startup. It is a driver
  limitation, not a design choice.
- **Plugins.** There is no plugin loader. [PLUGIN_API.md](docs/PLUGIN_API.md)
  describes the surface that exists and what a loader would need.
- **Keyboard focus, text input, and window management.** A bar that cannot take
  focus cannot be a launcher in the full sense. `onClick` works; `onKey` does not.

Nothing above is papered over. `shellctl doctor`, `shellctl actions` and the
startup log each say what is real.

## Licence

MIT. See [LICENSE](LICENSE).
