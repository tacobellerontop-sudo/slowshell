# Widgets

Eight element types. This is every property each one accepts, and what it does.

Every element also accepts the [common properties](#common-properties), which
are listed once at the end.

---

## Panel

A window attached to a screen edge. The top-level element: a config file must
contain at least one, and only a `Panel` may start a shell config.

```rust
Panel {
    position: "top"
    screen: "primary"
    exclusive: true
    backdrop: "acrylic"
    height: 32
    background: "#11111bcc"

    left   { /* anchored to the start  */ }
    center { /* centred                */ }
    right  { /* anchored to the end    */ }
}
```

| Property | Type | Meaning |
|---|---|---|
| `position` | enum | `top`, `bottom`, `left`, `right`, `floating`. Default `top`. |
| `screen` | enum | `primary`, `all`, or a display id. Default `primary`. |
| `exclusive` | bool | Reserve screen space so maximised windows stop at the panel. Default `false`. Ignored on a `floating` panel, which reserves nothing. |
| `wrap` | bool | Continue the panel's background around the other three screen edges, insetting the desktop on all four sides. Default `false`. Ignored on `floating` and `hidden`. |
| `wrapSize` | number | Thickness of the wrapped edges in logical pixels. Default `6`. |
| `backdrop` | enum | `none`, `acrylic`, `mica`, `micaAlt`, `blur`. |
| `alwaysOnTop` | bool | Keep above other windows. Default `true`. |
| `clickThrough` | bool | Ignore all pointer input. |
| `layer` | int | Stacking order among the shell's own surfaces. |
| `anchorX` | number | Horizontal position when `position: "floating"`, 0 to 1. |
| `anchorY` | number | Vertical position when `position: "floating"`, 0 to 1. |
| `name` | string | A name for `shell.open("name")`. |

With `exclusive: true` the panel is measured and placed against the display's
**work area** rather than its edges, which is what lets it share an edge with the
real taskbar: the work area already excludes every other bar, so a bottom bar
lands above Explorer's taskbar instead of through it. Two exclusive panels on
one edge stack. `shellctl screens` shows the result.

An exclusive panel is also part of its window's identity for hot reload, so
turning `exclusive` on or off and reloading recreates that window rather than
patching it. Without that, turning it off would leave the space reserved with
nothing on screen holding it.

`exclusive: true` and `height: 40` are both set in the starter config, so a fresh
install reserves space out of the box. The starter config is written once, on
first run, and never rewritten — `Shell.exe --starter` writes the *current*
defaults to a new file so you can see what has changed since your copy was made.

## `reveal`

`reveal: true` starts the panel collapsed to a thin strip and opens it when the
pointer enters — the dock-edge gesture Caelestia Shell uses.

```rust
Panel {
    position: "left"    // only left or right: it needs an edge to hug
    width: 210          // how wide it is when open
    reveal: true
    revealSize: 8       // how wide it is closed, i.e. what you hover
    revealDuration: 180 // ms
    revealDelay: 260    // ms to wait after the pointer leaves
    revealEase: "outCubic"
}
```

The window is *created* at the strip rather than created wide and hidden. That is
the whole trick: a window with no width cannot be hovered, so a panel that opened
from nothing could never be reopened.

Two details that are not obvious and that cost a real bug each:

- **`revealDelay` is load-bearing.** Without it, moving the pointer diagonally out
  of a panel that is still opening closes it mid-flight, and the far side of your
  own menu is unreachable. 260 ms is enough to cross an opening panel and short
  enough that the menu does not feel stuck.
- **A collapsed panel is empty, and an empty panel is click-through.** The shell
  reports `HTTRANSPARENT` where nothing is clickable, so an 8px strip with its
  content squeezed out would never see the pointer at all — the feature would fail
  completely and silently. A revealing panel therefore sets `hover_capture`, which
  makes the window *told* where the pointer is without making it interactive:
  clicks are still routed by region and still fall through empty areas.

Only `left` and `right` reveal this way. On `top` or `bottom` the panel's width is
the display's, and collapsing it would leave a full-width gap in the middle of the
desktop rather than a strip at the edge.

Easing curves live in `slowshell-core/src/ease.rs` and are unit-tested on their
own, because "does this feel right" is otherwise only answerable by eye — and by
eye it is answered wrongly on a 60 Hz panel and correctly on a 144 Hz one. The
curve matters more than the duration: `outCubic` spends most of its movement early
and settles at the end, which is what reads as responsive. `outBack` overshoots
slightly and reads as having weight.

Reveals cost nothing when settled: the frame loop asks whether anything is moving,
and a settled panel answers with one branch per surface before the loop goes back
to blocking in the kernel.

Changing `wrap` or `reveal` on reload recreates the panel's windows, because both
change how many windows there are or what they hold.

## `wrap`

`wrap: true` continues the panel's background around the other three edges, so
the desktop ends up inset on all four sides:

```rust
Panel {
    position: "top"  height: 46  exclusive: true
    wrap: true       // the shell builds the other three edges
    wrapSize: 6      // how thick they are
    background: "#151821f2"
}
```

It is one line in a config and four windows on screen. That is deliberate rather
than an implementation detail leaking out: the obvious way to draw a frame around
the desktop is a single full-screen window, and it cannot work on this
presentation path, which rejects per-pixel alpha and would paint an opaque
rectangle over everything. Four edge-anchored windows produce the same picture,
each reserves its own strip through the normal app bar path, and each is
destroyed by the same reload logic as any other panel.

The arms tile the work area without overlapping or leaving a gap, which is
asserted in `the_arms_tile_the_frame_without_gaps_or_overlaps` rather than left
to the eye — an overlap shows as a seam and a gap as a stripe of desktop, and
both read as a rendering bug rather than a geometry one. An arm carries no
children, so it has no hit regions and clicks fall through to whatever is
underneath.

Changing `wrap` on reload recreates the panel's windows, because the arm count is
part of the scene.
| `hidden` | bool | Compile the panel but give it no window until opened. |

### Bands

A `Panel` takes named child groups instead of an ordered list, because a bar has
bands and a `Row` does not:

| Group | Placement |
|---|---|
| `left` (or `start`) | At the start edge. |
| `center` | Centred. |
| `right` (or `end`) | At the end edge. |
| `content` | Fills the panel. |

Bands are placed independently, and shrink proportionally when the panel is too
narrow for all of them. A narrow window gets three cramped bands rather than two
bands and a gap.

A `Panel` also accepts positional children, which are treated as `content`.

### Sizing

An edge-anchored panel spans the width (or height) of its display, because a top
bar that stops short of the screen edge is not a top bar. Its cross-axis size
comes from the content's real measured height, which is why a bar with a `Clock`
is taller than a bar with nothing in it.

Give `width` and `height` to override. On a `floating` panel both are required —
there is no edge to be measured from.

### Hidden panels

```rust
Panel {
    name: "launcher"
    hidden: true
    position: "top"
    width: 520
    height: 46
    anchorX: 0.5
    content { /* … */ }
}
```

The panel is parsed, compiled and validated on every reload, and gets **no
window**. `shell.open("launcher")` — from a click, or from
`shellctl open launcher` — builds the window on demand.

This is the whole launcher mechanism. The renderer has no concept of a launcher;
a launcher is a panel in the config with a name and a `hidden` flag.

A config in which *every* panel is hidden produces no surfaces, and the shell
says so and exits rather than sitting there with nothing to draw.

---

## Row

Children left to right.

| Property | Type | Meaning |
|---|---|---|
| `gap` | number | Space between children. |
| `spacing` | number | Alias for `gap`. |
| `cross` | enum | `start`, `center`, `end`, `stretch`. Alignment across the row. |
| `justify` | enum | `start`, `center`, `end`, `space-between`. Distribution along it. |

```rust
Row {
    gap: 10
    padding: [0, 12]
    cross: "center"
    Text { text: "Apps"  fontWeight: 600 }
    Text { text: "|"  color: "border" }
    Spacer { }
    Clock { format: "HH:mm" }
}
```

---

## Column

Children top to bottom. Same properties as `Row`.

| Property | Type | Meaning |
|---|---|---|
| `gap` | number | Space between children. |
| `spacing` | number | Alias for `gap`. |
| `cross` | enum | `start`, `center`, `end`, `stretch`. Alignment across the column. |
| `justify` | enum | `start`, `center`, `end`, `space-between`. Distribution down it. |

---

## Container

A vertical stack sized to its **tallest child** rather than to the sum of them,
which is what makes it useful for a group that may be empty or may hold one item.

Takes exactly the layout properties of `Column`, and behaves like one. The only
difference is how it measures itself, so prefer it when the height is unknown
and a `Column` when it is not.

| Property | Type | Meaning |
|---|---|---|
| `gap` | number | Space between children. |
| `spacing` | number | Alias for `gap`. |
| `cross` | enum | `start`, `center`, `end`, `stretch`. |
| `justify` | enum | `start`, `center`, `end`, `space-between`. |

```rust
Container {
    gap: 6
    padding: 10
    background: "surface"
    radius: 8
    Text { text: "CPU"  color: "foregroundMuted" }
    Progress { value: 0.4  height: 6  color: "accent" }
}
```

---

## Spacer

Empty space. Gives its siblings room without a number to guess.

| Property | Type | Meaning |
|---|---|---|
| `width` | size | Main-axis extent. |
| `height` | size | Cross-axis extent. |

```rust
Row { Text { text: "left" }  Spacer { }  Text { text: "right" } }
Row { Text { text: "gap" }   Spacer { width: 12 }  Text { text: "here" } }
```

A bare `Spacer { }` fills whatever is left on the main axis, which is what makes
`justify: "start"` and `"end"` behave as their names suggest.

---

## Text

A string.

| Property | Type | Meaning |
|---|---|---|
| `text` | string | The string to display. May be an expression. |

```rust
Text { text: "Apps"  fontWeight: 600 }
Text { text: battery.percentage + "%"  visible: battery.present }
Text { text: "offline"  color: "warning"  visible: !network.connected }
```

`text` is the property that makes a config reactive. Anything that is not a
literal is an expression, and every system path it reads becomes a dependency
that is tracked and updated when the value changes.

Text is not wrapped or ellipsised. A bar has a fixed cross-axis size, so a long
string is clipped rather than reflowed. Keep bar text short, or use two widgets
with `visible` conditions.

---

## Clock

A live clock.

| Property | Type | Meaning |
|---|---|---|
| `format` | string | `strftime`-style format. |

### Formats

| | | | |
|---|---|---|---|
| `HH` | 24-hour, `00`–`23` | `hh` | 12-hour, `01`–`12` |
| `mm` | Minutes | `ss` | Seconds |
| `d` | Day, `1`–`31` | `D` | Day name, `Mon` |
| `a` | `AM`/`PM` | `A` | `AM`/`PM` (upper) |
| `M` | Month, `1`–`12` | `b` | Month name, `Jan` |
| `y` | Year | `p` | `AM`/`PM`, lowercase |

`-` and `,` are copied through, so `"D, d MMM"` and `"HH:mm:ss"` both work.

```rust
Clock { format: "HH:mm" }
Clock { format: "D MMM" }
Clock { format: "ddd, HH:mm" }
```

A format with none of the tokens in it is a **diagnostic**, not a blank widget.
That check exists because it caught a real bug: a `Clock` measured as an empty
string, collapsed the bar, and looked like a layout problem.

Each `Clock` is an independent element, so two of them in one bar cost nothing
extra and never need to be reconciled.

---

## Progress

A filled bar.

| Property | Type | Meaning |
|---|---|---|
| `value` | number | Fraction from 0 to 1. |
| `progress` | number | Alias for `value`. |
| `height` | size | Thickness of the bar. |
| `radius` | number | Rounded ends. |
| `color` | colour | The filled portion. |
| `background` | colour | The track behind it. |

```rust
Progress { value: 0.4  height: 6  radius: 3  color: "accent" }
Progress { value: system.memoryUsage / 100  height: 6  color: "success" }
```

`value` is a reactive expression like any other, so a progress bar that reads a
provider animates the moment that provider becomes live — with no change to the
config.

---

## Common properties

Every element accepts all of these.

| Property | Type | Meaning |
|---|---|---|
| `color` | colour | Text and icon colour. **Cascades to children.** |
| `foreground` | colour | Alias for `color`. |
| `background` | colour | Fill behind the element. |
| `border` | colour | Outline colour. |
| `borderWidth` | number | Outline thickness. |
| `borderRadius` | number | Corner radius. |
| `radius` | number | Alias for `borderRadius`. |
| `padding` | edges | Space inside. |
| `margin` | edges | Space outside. |
| `opacity` | number | 0 to 1, composes with children. |
| `width` | size | Main-axis width, or `fill`. |
| `height` | size | Cross-axis height. |
| `minWidth` | number | Smallest allowed width. |
| `minHeight` | number | Smallest allowed height. |
| `fontSize` | number | Text size in logical pixels. |
| `fontWeight` | integer | 100 to 900. |
| `fontFamily` | string | Font family name. |
| `letterSpacing` | number | Extra space between glyphs. |
| `textAlign` | enum | `start`, `center`, `end`. |
| `shadow` | string | Shadow preset name. |
| `visible` | bool | Whether the element takes part in layout. |
| `onClick` | call | Runs when the element is clicked. |
| `onShow` | call | Runs when the element becomes visible. |
| `onHide` | call | Runs when the element is hidden. |

### What cascades, and what does not

Only **typography and colour** cascade from a parent to its children: `color`,
`fontSize`, `fontWeight`, `fontFamily`, `letterSpacing`, `textAlign`.

`padding`, `margin`, `background`, `border` and `opacity` do **not**.

This is not an oversight. Inheriting geometry would hand every child the
parent's padding, and the layout pass already insets for the parent's padding —
so it would be counted twice, and every child would be inset by twice what you
wrote. That was a real bug, found by measuring rectangles in a test.

So `color` on a `Row` styles the whole subtree, and `padding` on a `Row` styles
only the `Row`.

### Sizes

`width` and `height` take a number of logical pixels, `fill`, or `auto`.

Logical pixels means the number is in layout units and scales with the display:
`height: 32` is 32 points at 100%, 40 physical pixels at 125%, and the bar is
the same apparent size on every monitor in a mixed-DPI setup.

`fill` takes all the space available on that axis. `auto` means "whatever the
content needs", which is the default.

### Handlers

```rust
Text { text: "Apps"  onClick: shell.open("launcher") }
```

An element is clickable only if it has a handler. Without one, its region is not
published for hit testing and the pointer passes straight through to whatever is
underneath — which is why the default config is entirely click-through until you
add a handler.

A handler must be a *call*. `onClick: "launcher.open"` is a string, would never
run, and is a diagnostic that says so.

Handler targets are checked at build time, so `onClick: launhcer.open()` is an
error next to the line that contains it, not a button that quietly does nothing.
See [ACTIONS.md](ACTIONS.md).

---

## Composition

### Bands and rows together

```rust
Panel {
    position: "top"
    height: 36
    left   { Row { gap: 8   Text { text: "Apps"  fontWeight: 600 } } }
    center { Row {          Clock { format: "HH:mm" } } }
    right  { Row { gap: 14  Text { text: "100%"  color: "foregroundMuted" } } }
}
```

### A section header

`letterSpacing` and a small size turn a string into a label:

```rust
Text {
    text: "DESKTOP"
    color: "foregroundSubtle"
    fontSize: 10
    fontWeight: 700
    letterSpacing: 1.2
}
```

### A button-ish label

There is no `Button` element. A `Text` with a background, padding, a radius and
a handler is a button, and the reason is that a `Button` would need to decide
what a button *is* — pressed states, keyboard focus, a default accent — and every
one of those decisions would be worse than the four properties you can already
set.

```rust
Text {
    text: "reload"
    padding: [6, 12]
    background: "surfaceAlt"
    radius: 7
    onClick: shell.reload()
}
```

### Spacing between things

`gap` on the parent, or `margin` on the child. `gap` is usually better, because
it puts the space where it belongs:

```rust
Row { gap: 10  A  B  C }     // ten between each pair
Row { A  B  C  margin: 10 }  // ten around each, so twenty between pairs
```
