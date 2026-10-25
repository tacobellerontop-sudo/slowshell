# Kerys

[Kerys](https://github.com/tacobellerontop-sudo/kerys) is the global hotkey
daemon for [rustWM](https://github.com/tacobellerontop-sudo/rustWM). It owns the
keyboard: a low-level hook, chords, and binding modes like Hyprland submaps.
Slowshell owns pixels. They work together without either needing the other to
be running.

```text
  keyboard ──► kerys.exe ──{"cmd":"focus left"}──────────────► rustwm.exe
                   │
                   ├──{"cmd":"toggle","name":"launcher"}──┐
                   └──{"cmd":"set","key":"kerys.mode",…}──┴──► Shell.exe
                                                                   │
            kerys.running, kerys.start() … ◄── FindWindowExW ──────┘
```

`examples/kerys.config` is a complete bar that uses all of this.

## Hotkeys that drive the shell

A Kerys binding whose command starts with `shell` is sent to the shell's control
pipe, the same one `shellctl` uses:

```toml
# %USERPROFILE%\.config\kerys\kerys.toml
[bindings]
"win+space"   = "shell toggle launcher"
"win+shift+r" = "shell reload"
"win+alt+o"   = "shell overlay"
```

| Command | Does |
|---|---|
| `shell toggle <name>` | Open a hidden panel if it is closed, close it if it is open. |
| `shell open <name>` / `shell close <name>` | Just one direction. |
| `shell reload` | Re-read the config. |
| `shell overlay` | The developer error overlay. |
| `shell stop` | Quit the shell. |

Kerys finds the shell the way `shellctl` does, through
`%LOCALAPPDATA%\Slowshell\control.json`, so no pipe name has to be configured.
If no shell is running the binding does nothing and Kerys logs why.

## Showing the binding mode

Kerys tells the shell every time its mode changes, and when it starts. In a
config that is `kerys.mode`:

```rust
Text {
    text: kerys.mode
    color: "warning"
    visible: kerys.running && kerys.mode != "default"
}
```

That is the whole submap indicator: it appears when Win+Alt+R puts Kerys in
resize mode and disappears on Escape. If Kerys exits, the shell notices on its
next one-second tick and puts `kerys.mode` back to `"default"`.

## Starting and stopping Kerys from the bar

`kerys.running` is whether Kerys is running, and four actions control it:
`kerys.start()`, `kerys.stop()`, `kerys.toggle()` and `kerys.settings()`, which
opens the keybinding editor. See [ACTIONS.md](ACTIONS.md#kerys).

## From anything else

What Kerys sends is ordinary `shellctl` protocol, so a script can do the same:

```powershell
shellctl toggle launcher
shellctl set kerys.mode resize
```

`set` only accepts keys under namespaces that belong to another program
(currently `kerys.`). The battery, clock and the rest are the shell's own
readings, and a script that could overwrite them could make the bar lie.

The wire format is one line of JSON each way:

```text
→ {"cmd":"toggle","name":"launcher"}
← {"status":"ok","data":{"name":"launcher","open":true}}
→ {"cmd":"set","key":"kerys.mode","value":"resize"}
← {"status":"ok","data":{"key":"kerys.mode","changed":true}}
```
