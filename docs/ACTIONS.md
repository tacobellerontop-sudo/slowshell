# Actions

An action is something a config may *call*. Config reads the system through
paths; it changes the system through actions.

```rust
Text { text: "Apps"  onClick: shell.open("launcher") }
Text { text: "vol +"  onClick: audio.raise() }
```

`shellctl actions` prints this list without needing a running shell.
`shellctl status` lists everything the *running* shell has registered, which is
the authoritative answer.

---

## Why actions and not properties

Config can read `battery.percentage` as a bare path. It cannot *write* with one.

That is deliberate. Reading is allowed from inside a reactive expression, so a
bare write hidden behind the same accessor would mean a future
`screens.width = 1920` typo silently became a write to a system object during
layout. Actions are always a **call**, never a bare name, which makes the
destructive half of the language visible at the call site.

## There are no built-in functions

A `name(...)` is always an action. There is no general-purpose function library,
and `min(1, 2)` is not a call to a builtin — it is a call to an action named
`min`, which the registry will tell you does not exist.

A config that can only call a known, documented set of actions is easier to
reason about, easier to check, and easier to restrict later.

---

## Shell control

These change what is on screen, so they are queued and carried out by the frame
loop rather than run inline. An action that rebuilt a surface while the paint
pass was walking it would be a use-after-free rather than a feature.

| Action | Does |
|---|---|
| `shell.reload()` | Re-read the config and rebuild the scene. |
| `shell.stop()` | Quit the shell. |
| `shell.overlay()` | Show or hide the developer error overlay. |
| `shell.open(name)` | Open a named surface, e.g. `"launcher"`. |

`shell.open` is the launcher mechanism. It builds the window for a panel
declared `hidden: true`, and re-shows it if it is already open. A name that is
not declared is an error naming what to add:

```
$ shellctl open nope
shellctl: the shell said: no panel named `nope` is declared.
Add `name: "nope" hidden: true` to a Panel, or check the name.
```

---

## The clipboard

| Action | Does |
|---|---|
| `clipboard.set(text)` | Copy text, replacing what was there. |
| `clipboard.get()` | Read text. Empty or non-text is `null`, not an error. |

```rust
Text { text: "copy"  onClick: clipboard.set("launcher") }
```

`clipboard.get()` returns a value, so it works in an expression as well as a
handler — though reading the clipboard on every frame would be a strange thing to
ask for.

Opening the clipboard retries briefly if something else has it. Windows allows
one process to hold it, and plenty of things do; giving up on the first refusal
would turn a momentary overlap into a lost copy.

---

## Launching things

| Action | Does |
|---|---|
| `launch(target)` | Open a file, URL or program, as if typed into Run. |
| `exec.run(program, args)` | Run a program and wait, returning its exit code. |

```rust
Text { text: "open"  onClick: launch("notepad.exe") }
Text { text: "build"  onClick: exec.run("cargo", ["build", "--release"]) }
```

`launch` hands the string to the shell verbatim, so a `.txt` becomes Notepad, a
URL becomes the browser, and a bare name becomes a program lookup. That is what
"Run" means, and reimplementing the association lookup would only get it wrong.

`exec.run` passes arguments as a list, quoted per the documented Win32 rule, so
a path with a space needs no quoting from the config and there is no way to
inject an extra argument. It waits up to 60 seconds; a program that has not
exited by then is reported rather than left hanging the shell's action.

A misspelled program is an error naming the program. A launcher that silently
does nothing is the most annoying class of shell bug there is.

---

## Notifications

| Action | Does |
|---|---|
| `notify(title, body)` | Show a transient notification. |

```rust
Text { text: "test"  onClick: notify("Hello", "From slowshell") }
```

Windows has no single notification API, and the two that exist both need setup a
desktop shell does not get for free. `Shell_NotifyIcon` balloon tips need a tray
icon that outlives the call; toast notifications need a registered
AppUserModelID and a shortcut on disk, which only works for a packaged or
installed app.

Rather than fake either, `notify` draws a small always-on-top window in the
corner for a few seconds. It is the shell's own surface type, so it needs no
registration and works on every Windows 10 and 11 machine. The limitation is
worth stating: it belongs to the shell process, so it disappears when the shell
does, and it is not in the Action Center.

---

## Audio

| Action | Does |
|---|---|
| `audio.setVolume(level)` | Set the volume, 0 to 100. |
| `audio.mute(muted)` | Mute or unmute. |
| `audio.toggleMute()` | Mute if audible, unmute otherwise. |
| `audio.raise()` | Raise by five. |
| `audio.lower()` | Lower by five. |

**The limitation, stated plainly:** these write the volume and cannot read it
back. There is no "what is the volume" API on Windows that does not need a full
audio endpoint enumeration on a timer, and a timer is exactly the thing the idle
budget forbids. So the shell remembers what it last set and applies changes as
multimedia key presses.

That is coarse — `setVolume(37)` steps to the nearest multiple of twelve and is
honest about being approximate. A config that needs a live volume *gauge* cannot
have one from these actions. A config that needs a mute *button* can.

An out-of-range level is refused rather than clamped, because clamping would hide
a config that says `1000`, which is a mistake worth seeing.

---

## Windows

| Action | Does |
|---|---|
| `windows.minimiseAll()` | Minimise every window. |
| `windows.lock()` | Lock the workstation. |

`windows.lock` is `LockWorkStation`, which needs no privilege.

---

## Keyboard

| Action | Does |
|---|---|
| `keys.send(chord)` | Press a key chord, or a media key. |

```rust
Text { text: "show desktop"  onClick: keys.send("Win+D") }
Text { text: "play/pause"   onClick: keys.send("PlayPause") }
Text { text: "vol up"       onClick: audio.raise() }
```

Chords are `Win+D`, `Ctrl+Shift+P`, `Alt+F4`, `F12`, `Super+Space`. Modifier
names are `ctrl`/`control`, `alt`, `shift`, `super`/`win`/`meta`.

Media keys are `VolumeUp`, `VolumeDown`, `Mute`, `PlayPause`, `NextTrack`,
`PrevTrack`, `Stop`, `BrightnessUp`, `BrightnessDown`.

Input is **injected** rather than posted, because a posted `WM_KEYDOWN` is
ignored by anything that reads the keyboard state directly, and the things a
shell most wants to reach — `Win+D` and the volume keys — both do.

A key that has no sendable code is refused rather than guessed. Injecting the
wrong key is worse than injecting none.

---

## Screens

| Action | Does |
|---|---|
| `screens.list()` | The display names a `Panel` can be bound to. |

---

## Power

| Action | Does |
|---|---|
| `system.shutdown()` | Shut the machine down. |
| `system.restart()` | Restart the machine. |

These shell out to `shutdown.exe`, because `ExitWindowsEx` needs
`SE_SHUTDOWN_PRIVILEGE`, which a normal user process does not hold and cannot
enable for itself. The helper runs elevated on demand, or the call fails and says
so.

They are in the registry because a power menu is the first thing anyone builds,
and a shell that has no honest answer for "restart" forces people to reach for
the Start menu.

---

## Checking a handler

A handler's target is validated when the config is compiled, not when it is
clicked:

```
$ Shell.exe --check broken.config

Runtime Error

broken.config:2:23

`onClick`: `launhcer.open` is not an action.

A handler is a call. `onClick: launcher.toggle()` is correct; `onClick: "launcher.toggle"` is a string and never runs.
Registered actions: audio.lower, audio.mute, audio.raise, audio.setVolume, audio.toggleMute,
  clipboard.get, clipboard.set, exec.run, keys.send, launch, notify, screens.list,
  shell.open, shell.overlay, shell.reload, shell.stop, system.restart, system.shutdown,
  windows.lock, windows.minimiseAll
```

A misspelling gets a "did you mean" when something is genuinely close. When
nothing is close, no suggestion is offered: a confidently wrong suggestion is
worse than none.

`Shell.exe --check` uses the same registry the shell does, so a handler that
passes the check is a handler the shell can resolve.

---

## Arguments

A handler's arguments are evaluated **once, when the config is compiled**:

```rust
Text { text: "open notepad"  onClick: launch("notepad.exe") }
```

A handler fires on a click, long after the graph it was written in was torn
down, so a reactive argument would have nothing to recompute against. A literal
is what a config means anyway. If you want a live value, read it in a `Text`.

The wrong number of arguments is an error naming what it got and what it wanted:

```
`audio.setVolume` takes 1 (level), but was given 0.
```

---

## Adding one

The registry is `slowshell_core::Registry`, and it is a language-level concept:
the runtime validates against it, the shell fills it, and each platform module
registers its own half.

```rust
// in slowshell-win/src/platform.rs
registry.register(
    "system.lock",
    "Lock the workstation.",
    &[],
    |_| { /* … */ Ok(Value::str("locked")) },
);
```

`register` takes the path, a one-line description, the parameter names, and the
closure. The description is not decoration — `shellctl actions` prints it, so an
action without one is a hole in the user-facing surface. The parameter names are
used for the arity error message.

Nothing in `platform.rs` is reachable until `register` runs, which means a future
restricted mode has one switch to flip rather than a list of calls to audit.
