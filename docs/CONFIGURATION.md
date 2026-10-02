# Configuration

A Slowshell config is a single file of nested blocks. It is not JSON, not YAML,
and not TOML, because a config language should read like the thing it describes.

```rust
// Comments use `//`.
Theme { accent: "#7c9cff" }

Panel {
    position: "top"
    background: "background"
    left  { Text { text: "Apps"  fontWeight: 600 } }
    center { Clock { format: "HH:mm" } }
    right { Text { text: "WiFi"  color: "foregroundMuted" } }
}
```

A file must contain at least one `Panel`. Panels are the top-level thing: a
`Text` at the top level is an error, because a text label is not a desktop.

---

## Values

| Type | Examples |
|---|---|
| String | `"top"`, `"#7c9cff"`, `"HH:mm"` |
| Number | `12`, `0.5`, `-3` |
| Boolean | `true`, `false` |
| Colour | `#7c9cff`, `#7c9cff80`, `"accent"` |
| Edges | `8`, `fill`, `[0, 12]`, `[1, 2, 3, 4]` |
| Size | `120`, `fill`, `auto` |
| List | `[1, 2, 3]` |
| Call | `launch("notepad.exe")` |

### Colours

`#rgb`, `#rrggbb`, and `#rrggbbaa`. Written as a bare word, with no quotes:

```rust
background: "#11111bcc"
border:     "#ffffff1f"
accent:     "#7c9cff"
```

Alpha is `00` (transparent) to `ff` (opaque). On a machine where the driver
rejects alpha-aware swap chains, translucent colours are pre-composited against
the panel's own background and the panel is opaque. `Shell.exe --doctor` says
which case you are in.

A colour may also be a **theme token** in quotes, which is what you almost
always want:

```rust
color: "foreground"
color: "foregroundMuted"
color: "accent"
```

See [THEMES.md](THEMES.md) for the full palette. The point of tokens is that a
theme change retints everything, and a config that hardcodes hex does not
participate in that.

### Edges

`padding` and `margin` take a number, `fill`, or a list. The list forms are:

| | |
|---|---|
| `[a]` | all four sides |
| `[v, h]` | vertical, horizontal |
| `[t, r, b, l]` | top, right, bottom, left — clockwise from the top |
| `[t, h, b]` | top, horizontal, bottom |

```rust
padding: 8            // all sides
padding: [0, 12]      // 0 vertical, 12 horizontal
padding: [10, 0, 4]   // 10 top, 0 horizontal, 4 bottom
padding: fill         // as much as there is
```

---

## Elements

| Element | What it is |
|---|---|
| `Panel` | A window attached to a screen edge. The top-level thing. |
| `Row` | Children left to right. |
| `Column` | Children top to bottom. |
| `Container` | A vertical stack sized to its tallest child. Takes `Column`'s layout properties. |
| `Spacer` | Empty space that pushes its siblings apart. |
| `Text` | A string. |
| `Clock` | A live clock. |
| `Progress` | A filled bar. |

Properties per element are in [WIDGETS.md](WIDGETS.md).

### Panel bands

A `Panel` takes named child groups rather than an ordered list, because a bar has
bands and a `Row` does not:

```rust
Panel {
    position: "top"
    left   { /* anchored to the left / start  */ }
    center { /* centred                        */ }
    right  { /* anchored to the right / end    */ }
    content { /* fills the panel               */ }
}
```

`start` and `end` are accepted as aliases for `left` and `right`, for configs
that read better left-to-right-agnostic.

Bands are placed independently: `left` at the start, `center` centred, `right`
at the end. When a bar is too narrow for all three, the bands shrink
proportionally rather than overlapping or dropping one. A 320-pixel-wide window
gets three cramped bands, not two bands and a gap.

A `Panel` also accepts ordinary positional children, which are treated as
`content`.

### Widgets

```rust
Text { text: "Apps"  fontWeight: 600 }
Text { text: "87%"  color: "foregroundMuted"  visible: battery.present }
Clock { format: "HH:mm" }
Clock { format: "ddd D MMM" }
Progress { value: 0.4  height: 6  radius: 3  color: "accent" }
Spacer { }
Spacer { width: 12 }
```

`Clock` formats are `strftime`-style: `HH` `hh` `mm` `ss` `d` `D` `a` `A` `M` `b`
`y` `p`, with `-` and `,` copied through. A format with none of them is an
error rather than a blank widget — this caught a real bug where a `Clock`
measured as an empty string and collapsed the bar.

---

## Expressions

Anything that is not a literal is an expression.

### Operators

| | |
|---|---|
| `+` `-` `*` `/` `%` | arithmetic |
| `==` `!=` `<` `<=` `>` `>=` | comparison |
| `&&` `\|\|` | logic |
| `!` | negation |
| `-` | negation of a number |

There is no ternary operator. Use two elements and a `visible` condition, which
is both more readable and animatable later:

```rust
// Not this:
Text { text: battery.percentage < 20 ? "low" : "ok" }

// This:
Text { text: "low"  color: "error"  visible: battery.percentage < 20 }
Text { text: "ok"   color: "foregroundMuted"  visible: battery.percentage >= 20 }
```

### Reading system state

Any dotted path a provider exposes:

```rust
clock.time  clock.date  clock.unix  clock.weekday
screens.count  screens.primary.width  screens.<id>.height
battery.percentage  battery.charging  battery.present
network.connected  network.wifi  network.ssid
audio.volume  audio.muted
system.cpuUsage  system.memoryUsage  system.hostName
windows.active.title  windows.active.process
notifications.unreadCount
```

`screens.primary` is the primary display, and each display is also addressable by
its own id — `screens.1.width`, `screens.1.scale`. `shellctl screens` prints the
ids.

Every one of these is **tracked**. A widget that reads `battery.percentage` is
recomputed when the battery changes and not otherwise. A widget that reads
nothing is never recomputed at all.

Providers that are not implemented read as `null`. `Shell.exe --doctor` and
`shellctl doctor` list which ones, and why. Guard anything you use:

```rust
Text { text: battery.percentage + "%"  visible: battery.present }
```

### `+` is overloaded, on purpose

```rust
"Room " + n            // string concat
battery.percentage + "%"   // number then string: "87%"
"Charge: " + battery.percentage + "%"
```

And for colours, `+` lerps toward white (positive) or black (negative):

```rust
background: "accent" + 0.2      // a lighter accent
background: "accent" - 0.3      // a darker accent
```

### There are no built-in functions

A `name(...)` is always an *action*, never a builtin. `min(1, 2)` is not a
function call — it is an action called `min`, and the registry will tell you so,
with the list of things you can actually call. This is intentional: a config that
can only call a known, documented set of actions is easier to reason about than
one with a general-purpose function library hiding in it.

See [ACTIONS.md](ACTIONS.md).

### Handlers

```rust
Text { text: "Apps"  onClick: shell.open("launcher") }
Text { text: "open"  onClick: launch("notepad.exe") }
Text { text: "vol +"  onClick: audio.raise() }
```

A handler **must be a call**. `onClick: "launcher.open"` is a string, and it
would never run — so it is a diagnostic that says so.

Arguments are evaluated once, when the config is compiled. A handler fires on a
click, long after the graph it was written in was torn down, so a reactive
argument would have nothing to recompute against. A literal is what a config
means anyway.

Handler targets are checked at build time against the real registry, so a typo
is a diagnostic next to the line that contains it.

---

## Themes

A `Theme` block retints the palette. It is a config block, not a widget: it
contributes no surface.

```rust
Theme {
    name: "catppuccin"
    dark: true
    radius: 8
    fontSize: 13
    accent: "#89b4fa"
    background: "#1e1e2ef2"
    foreground: "#cdd6f4"
}
```

Every palette entry is a colour, and any colour may reference another token
rather than repeating a hex code. See [THEMES.md](THEMES.md).

`name`, `dark`, `radius` and `fontSize` are the shared metrics, and they are
checked before the colour path — which is why `name: "catppuccin"` is a name
and not an error about an unknown colour.

---

## Includes

```rust
include "panels/bar.config"
include "themes/dark.config"
```

Resolved relative to the including file, not the working directory, so a config
tree can be moved or symlinked without breaking. Every file read is watched, so
editing an include reloads the shell just as editing the root does.

---

## Errors

Diagnostics carry severity, file, line, column, a message, alternatives, a hint
and notes.

```
$ Shell.exe --check broken.config

Unknown Property

broken.config:2:23

Unknown property "foregorund" on Text

Did you mean "foreground"?
```

```
$ Shell.exe --check typo.config

Unknown Element

typo.config:2:5

Unknown element `Panle`

Did you mean "Panel"?
```

```
$ Shell.exe --check handler.config

Runtime Error

handler.config:2:23

`onClick`: `launhcer.open` is not an action.

A handler is a call. `onClick: launcher.toggle()` is correct; `onClick: "launcher.toggle"` is a string and never runs.
Registered actions: audio.lower, audio.mute, audio.raise, audio.setVolume, audio.toggleMute,
  clipboard.get, clipboard.set, exec.run, keys.send, launch, notify, screens.list,
  shell.open, shell.overlay, shell.reload, shell.stop, system.restart, system.shutdown,
  windows.lock, windows.minimiseAll
```

The action list is printed because "not an action" is much less useful without
knowing what the alternatives are. `shellctl actions` prints the same list
without a running shell.

A suggestion is only offered when something is genuinely close. `launhcer.open`
in the example above gets no "did you mean" because nothing in the registry is
near it, and a confidently wrong suggestion is worse than none.

A build with any error **keeps the previous scene running**. A half-typed line
never blanks your desktop; it logs, and `shellctl diagnostics` shows it. A
*warning* does not stop the build at all.

### Saving from a Windows editor

A config saved by Notepad or Visual Studio will usually start with a UTF-8 byte
order mark. That is stripped on load, because rejecting it would blame line 1
for something the user never typed. A U+FEFF *inside* a file is a real
character and is reported.

---

## A complete example

```rust
Theme {
    name: "my-bar"
    dark: true
    radius: 8
    accent: "#7c9cff"
    background: "#0f1117f2"
    foreground: "#e6e7ef"
    foregroundMuted: "#8b8fa3"
}

Panel {
    position: "top"
    exclusive: true
    height: 32
    background: "background"
    padding: 0

    left {
        Row {
            gap: 8
            padding: [0, 12]
            cross: "center"
            Text { text: "Apps"  fontWeight: 600  onClick: shell.open("launcher") }
            Text { text: "1"  fontWeight: 600 }
            Text { text: "2"  color: "foregroundSubtle" }
        }
    }

    center {
        Row { cross: "center"  Clock { format: "HH:mm"  fontWeight: 500 } }
    }

    right {
        Row {
            gap: 16
            padding: [0, 12]
            cross: "center"
            Text { text: battery.percentage + "%"  color: "foregroundMuted"  visible: battery.present }
            Text { text: "WiFi"  color: "foregroundMuted"  visible: network.connected }
            Text { text: "offline"  color: "warning"  visible: !network.connected }
            Clock { format: "D MMM"  color: "foregroundMuted" }
        }
    }
}

Panel {
    name: "launcher"
    hidden: true
    position: "top"
    width: 520
    height: 46
    anchorX: 0.5
    background: "surface"
    border: "borderStrong"
    borderWidth: 1
    radius: 12

    content {
        Row {
            gap: 14
            padding: [0, 16]
            cross: "center"
            Text { text: "Run"  color: "foregroundSubtle" }
            Text { text: "notepad.exe"  color: "foreground" }
            Text { text: "launch"  color: "accent"  fontWeight: 600  onClick: launch("notepad.exe") }
        }
    }
}
```

More in [`examples/`](../examples), all of which are compiled by the test suite so
they cannot rot.
