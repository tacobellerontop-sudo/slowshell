# Themes

A theme is a named palette. `Theme` is a config block, not a widget: it retints
the palette and contributes no surface.

```rust
Theme {
    name: "catppuccin"
    dark: true
    radius: 8
    fontSize: 13
    accent: "#89b4fa"
    background: "#1e1e2ef2"
}
```

Any entry may reference another token instead of repeating a hex code, so a
theme can derive one colour from another:

```rust
Theme {
    accent: "#7c9cff"
    accentHover: "accent" + 0.15    // a lighter accent
    accentPressed: "accent" - 0.15   // a darker accent
}
```

`+` and `-` on a colour lerp toward white and black respectively.

---

## Metrics

| Property | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | `slowshell-dark` | Shown in the log. Not a colour. |
| `dark` | bool | `true` | Whether the palette is a dark one. |
| `radius` | number | `10` | Default corner radius. |
| `fontSize` | number | `14` | Base type size, in logical pixels. |

These four are checked *before* the colour path, which is why `name: "catppuccin"`
is a name and not an error about an unknown colour.

---

## The default palette

| Token | Default | For |
|---|---|---|
| **Surfaces** | | |
| `background` | `#1a1b26f2` | The panel itself. |
| `surface` | `#242532f5` | A raised element inside a panel. |
| `surfaceAlt` | `#2e3040f7` | A hover or pressed state. |
| `overlay` | `#1e1f2bfa` | The developer error overlay. |
| **Text** | | |
| `foreground` | `#e6e7ef` | Primary text. |
| `foregroundMuted` | `#9a9db0` | Secondary text. |
| `foregroundSubtle` | `#6b6e80` | Labels, hints, disabled text. |
| **Accent** | | |
| `accent` | `#7c9cff` | The one interactive colour. |
| `accentHover` | `#93aeff` | Hover. |
| `accentPressed` | `#6383e0` | Pressed. |
| **Semantic** | | |
| `success` | `#6bd08c` | Something worked. |
| `warning` | `#e8c074` | Something needs attention. |
| `error` | `#f07a86` | Something failed. |
| `batteryLow` | `#f0a85a` | Battery under 20%. |
| `batteryCharging` | `#7cd08c` | Battery is charging. |
| **Lines** | | |
| `border` | `#ffffff0e` | A hairline. |
| `borderStrong` | `#ffffff1f` | A visible outline. |
| **Backdrop** | | |
| `scrim` | `#0d0e1499` | A dim behind a panel on a display that cannot do blur. |

A token that a theme does not mention keeps its default, so a theme that sets
five colours and nothing else is a complete, valid theme.

An unknown token name is a diagnostic that lists the ones that exist.

---

## Alpha

Colours carry alpha, `00` transparent to `ff` opaque:

```rust
background: "#11111bcc"     // 80% opaque
border:     "#ffffff1f"     // 12% opaque
scrim:      "#0d0e1499"     // 60% opaque
```

Where the driver accepts an alpha-aware swap chain, this is real per-pixel
transparency and DWM backdrops composite behind it.

Where it does not, the shell presents opaquely and **pre-composites** translucent
colours against the panel's own background, so a `80%` opaque panel looks like an
`80%` opaque panel rather than a solid one with the wrong colour. The alpha is
not silently discarded — it is flattened, and `Shell.exe --doctor` says which
case you are in.

---

## Backdrops

```rust
Panel {
    position: "top"
    backdrop: "acrylic"     // none | acrylic | mica | micaAlt | blur
    background: "#11111bcc"
}
```

| | |
|---|---|
| `mica` | The material Windows uses behind a normal app window. Tinted by the system. |
| `micaAlt` | The alternate mica, which uses the system accent. |
| `acrylic` | Translucent, with a noise texture. |
| `blur` | A plain Gaussian blur of whatever is behind. |
| `none` | Nothing. The default. |

A backdrop only has a visible effect on a path that presents per-pixel alpha. On
the opaque fallback it is dropped, because compositing a blur under a solid
background would draw the blur over the bar and look like a rendering fault.

`shellctl doctor` reports whether backdrops are available on this build.

---

## A light theme

The palette is not hardcoded to dark, and `dark: false` changes what the shell
asks the DWM for.

```rust
Theme {
    name: "paper"
    dark: false
    radius: 6
    fontSize: 14
    background: "#f6f5f2f2"
    surface: "#ffffffff"
    surfaceAlt: "#eceae5"
    foreground: "#23231f"
    foregroundMuted: "#6a6862"
    foregroundSubtle: "#9a978f"
    accent: "#3b6ea5"
    accentHover: "#4b7cb0"
    accentPressed: "#2f5c8c"
    success: "#3f8a54"
    warning: "#a8762a"
    error: "#b3402f"
    border: "#00000014"
    borderStrong: "#00000026"
}
```

The two things worth noticing: the border tokens are *black* at low alpha rather
than white, because a white border is invisible on a light surface; and
`batteryLow` is left alone, because an orange that reads on a dark panel reads on
a light one too.

---

## A generated theme

Palette entries are just colours, and a config has arithmetic, so a theme can
derive a whole ramp from one hue:

```rust
Theme {
    background: "#101219f2"
    surface:    "background" + 0.05
    surfaceAlt: "surface" + 0.05
    foreground: "#e6e7ef"
    foregroundMuted: "foreground" - 0.30
    foregroundSubtle: "foreground" - 0.55
    accent: "#cba6f7"
    success: "#a6e3a1"
    warning: "#f9e2af"
    error:   "#f38ba8"
    border:  "#ffffff14"
    borderStrong: "#ffffff24"
}
```

`+` lerps toward white, `-` toward black, and both clamp at the ends.

---

## Tokens versus literals

Use tokens in your config and literals only in your theme. A config that
hardcodes `#e6e7ef` will not follow a theme change, which defeats the point of
having one:

```rust
// This follows the theme.
Text { text: "Apps"  color: "foreground" }

// This does not.
Text { text: "Apps"  color: "#e6e7ef" }
```

The one place a literal is right is a genuinely one-off colour that no palette
could name — a specific brand colour, a syntax highlight. Put those in the
theme anyway, under a name that says what they are.
