//! Keyboard and pointer input, normalised away from raw Win32 virtual keys.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, keybd_event, VK_BACK, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_HOME, VK_LEFT, VK_LCONTROL, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SPACE, VK_TAB, VK_UP,
};

/// Modifier keys held while an event was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const NONE: KeyModifiers = KeyModifiers(0);
    pub const SHIFT: KeyModifiers = KeyModifiers(1 << 0);
    pub const CTRL: KeyModifiers = KeyModifiers(1 << 1);
    pub const ALT: KeyModifiers = KeyModifiers(1 << 2);
    pub const SUPER: KeyModifiers = KeyModifiers(1 << 3);

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// True when every flag in `other` is set on `self`.
    pub const fn contains(self, other: KeyModifiers) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for KeyModifiers {
    type Output = KeyModifiers;
    fn bitor(self, rhs: Self) -> Self {
        KeyModifiers(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for KeyModifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for KeyModifiers {
    type Output = KeyModifiers;
    fn bitand(self, rhs: Self) -> Self {
        KeyModifiers(self.0 & rhs.0)
    }
}

/// A logical key, independent of the active keyboard layout's scancodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Tab,
    Space,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    F(u8),
    Other,
}

impl Key {
    /// The name used in config, matching what a user would write in a `Hotkey`.
    pub fn name(&self) -> String {
        match self {
            Key::Char(c) => c.to_string(),
            Key::Enter => "Return".into(),
            Key::Escape => "Escape".into(),
            Key::Tab => "Tab".into(),
            Key::Space => "Space".into(),
            Key::Backspace => "Backspace".into(),
            Key::Left => "Left".into(),
            Key::Right => "Right".into(),
            Key::Up => "Up".into(),
            Key::Down => "Down".into(),
            Key::PageUp => "PageUp".into(),
            Key::PageDown => "PageDown".into(),
            Key::Home => "Home".into(),
            Key::End => "End".into(),
            Key::Delete => "Delete".into(),
            Key::F(n) => format!("F{n}"),
            Key::Other => "Unknown".into(),
        }
    }
}

/// A pointer or keyboard event in window-local logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    Moved { x: f32, y: f32, modifiers: KeyModifiers },
    Pressed { x: f32, y: f32, button: u8, modifiers: KeyModifiers },
    Released { x: f32, y: f32, button: u8, modifiers: KeyModifiers },
    Wheel { delta: f32, modifiers: KeyModifiers },
    Key { key: Key, pressed: bool, repeat: bool, modifiers: KeyModifiers },
    Left,
}

/// Translate a Win32 virtual key code into a logical key.
pub fn key_from_vk(vk: u16) -> Key {
    match vk {
        x if x == VK_RETURN.0 as u16 => Key::Enter,
        x if x == VK_ESCAPE.0 as u16 => Key::Escape,
        x if x == VK_TAB.0 as u16 => Key::Tab,
        x if x == VK_SPACE.0 as u16 => Key::Space,
        x if x == VK_BACK.0 as u16 => Key::Backspace,
        x if x == VK_LEFT.0 as u16 => Key::Left,
        x if x == VK_RIGHT.0 as u16 => Key::Right,
        x if x == VK_UP.0 as u16 => Key::Up,
        x if x == VK_DOWN.0 as u16 => Key::Down,
        x if x == VK_PRIOR.0 as u16 => Key::PageUp,
        x if x == VK_NEXT.0 as u16 => Key::PageDown,
        x if x == VK_HOME.0 as u16 => Key::Home,
        x if x == VK_END.0 as u16 => Key::End,
        x if x == VK_DELETE.0 as u16 => Key::Delete,
        0x30..=0x39 => Key::Char((vk as u8) as char), // 0-9
        0x41..=0x5a => Key::Char((vk as u8) as char), // A-Z
        0x70..=0x87 => Key::F((vk - 0x6f) as u8),
        _ => Key::Other,
    }
}

/// Parse `"Super+Space"` into modifiers plus a key.
///
/// Returns `None` for anything unparseable so a bad hotkey is reported at config
/// time rather than silently never firing.
pub fn parse_hotkey(spec: &str) -> Option<(KeyModifiers, Key)> {
    let mut mods = KeyModifiers::NONE;
    let mut key = None;
    for part in spec.split('+') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CTRL,
            "alt" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "super" | "win" | "meta" => mods |= KeyModifiers::SUPER,
            "space" => key = Some(Key::Space),
            "return" | "enter" => key = Some(Key::Enter),
            "escape" | "esc" => key = Some(Key::Escape),
            "tab" => key = Some(Key::Tab),
            "left" => key = Some(Key::Left),
            "right" => key = Some(Key::Right),
            "up" => key = Some(Key::Up),
            "down" => key = Some(Key::Down),
            "backspace" => key = Some(Key::Backspace),
            "pageup" => key = Some(Key::PageUp),
            "pagedown" => key = Some(Key::PageDown),
            "home" => key = Some(Key::Home),
            "end" => key = Some(Key::End),
            "delete" | "del" => key = Some(Key::Delete),
            other => {
                if let Some(rest) = other.strip_prefix('f') {
                    if let Ok(n) = rest.parse::<u8>() {
                        if (1..=12).contains(&n) {
                            key = Some(Key::F(n));
                            continue;
                        }
                    }
                }
                // A single character is a literal key.
                let mut chars = p.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => key = Some(Key::Char(c)),
                    _ => return None,
                }
            }
        }
    }
    key.map(|k| (mods, k))
}

/// Map a logical key back to a virtual key code for `RegisterHotKey`.
pub fn key_to_vk(key: Key) -> u16 {
    match key {
        Key::Char(c) => {
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_alphanumeric() {
                upper as u16
            } else {
                match c {
                    ' ' => VK_SPACE.0 as u16,
                    '\n' => VK_RETURN.0 as u16,
                    '\t' => VK_TAB.0 as u16,
                    _ => 0,
                }
            }
        }
        Key::Enter => VK_RETURN.0 as u16,
        Key::Escape => VK_ESCAPE.0 as u16,
        Key::Tab => VK_TAB.0 as u16,
        Key::Space => VK_SPACE.0 as u16,
        Key::Backspace => VK_BACK.0 as u16,
        Key::Left => VK_LEFT.0 as u16,
        Key::Right => VK_RIGHT.0 as u16,
        Key::Up => VK_UP.0 as u16,
        Key::Down => VK_DOWN.0 as u16,
        Key::PageUp => VK_PRIOR.0 as u16,
        Key::PageDown => VK_NEXT.0 as u16,
        Key::Home => VK_HOME.0 as u16,
        Key::End => VK_END.0 as u16,
        Key::Delete => VK_DELETE.0 as u16,
        Key::F(n) => 0x6f + n as u16,
        Key::Other => 0,
    }
}

/// Win32 modifier flags for `RegisterHotKey`.
pub fn hotkey_modifiers(mods: KeyModifiers) -> u32 {
    let mut out = 0;
    if mods.contains(KeyModifiers::ALT) {
        out |= 0x0001; // MOD_ALT
    }
    if mods.contains(KeyModifiers::CTRL) {
        out |= 0x0002; // MOD_CONTROL
    }
    if mods.contains(KeyModifiers::SHIFT) {
        out |= 0x0004; // MOD_SHIFT
    }
    if mods.contains(KeyModifiers::SUPER) {
        out |= 0x0008; // MOD_WIN
    }
    // Without NO_REPEAT a held key fires repeatedly, which a toggle action such as
    // a launcher opening would find very confusing.
    out |= 0x4000; // MOD_NOREPEAT
    out
}

/// Human-readable modifier list, used in error messages.
pub fn describe_modifiers(mods: KeyModifiers) -> String {
    let mut parts = Vec::new();
    if mods.contains(KeyModifiers::CTRL) {
        parts.push("Ctrl");
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("Alt");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("Shift");
    }
    if mods.contains(KeyModifiers::SUPER) {
        parts.push("Super");
    }
    parts.join("+")
}

/// Windows virtual key codes referenced by the platform layer, kept here so the
/// public surface does not leak the `windows` crate.
pub mod vk {
    // A nested module does not inherit the parent's imports.
    use super::*;

    pub const SHIFT: u16 = VK_LSHIFT.0 as u16;
    pub const CONTROL: u16 = VK_LCONTROL.0 as u16;
    pub const ALT: u16 = VK_MENU.0 as u16;
    pub const LWIN: u16 = VK_LWIN.0 as u16;
    pub const RWIN: u16 = VK_RWIN.0 as u16;
    pub const RSHIFT: u16 = VK_RSHIFT.0 as u16;
    pub const RCTRL: u16 = VK_RCONTROL.0 as u16;
    pub const RMENU: u16 = VK_RMENU.0 as u16;
}

/// Whether a virtual key is currently held. The high bit of `GetKeyState` is the
/// "physically down" flag.
pub fn key_is_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

/// The virtual key for a logical key, for injecting a chord.
///
/// Only the keys a shell might usefully press are here. Anything else returns
/// `None` rather than a wrong code, because injecting the wrong key is worse
/// than injecting none.
pub fn vk_of(key: Key) -> Option<u16> {
    Some(match key {
        Key::Enter => VK_RETURN.0 as u16,
        Key::Escape => VK_ESCAPE.0 as u16,
        Key::Tab => VK_TAB.0 as u16,
        Key::Space => VK_SPACE.0 as u16,
        Key::Backspace => VK_BACK.0 as u16,
        Key::Left => VK_LEFT.0 as u16,
        Key::Right => VK_RIGHT.0 as u16,
        Key::Up => VK_UP.0 as u16,
        Key::Down => VK_DOWN.0 as u16,
        Key::PageUp => VK_PRIOR.0 as u16,
        Key::PageDown => VK_NEXT.0 as u16,
        Key::Home => VK_HOME.0 as u16,
        Key::End => VK_END.0 as u16,
        Key::Delete => VK_DELETE.0 as u16,
        Key::Char(c) if c.is_ascii_digit() => c as u16,
        Key::Char(c) if c.is_ascii_uppercase() => c as u16,
        Key::Char(c) if c.is_ascii_lowercase() => (c as u16) - 32,
        Key::F(n) if (1..=12).contains(&n) => 0x6f + n as u16,
        _ => return None,
    })
}

/// Press and release a key chord, e.g. `Win+D`.
///
/// Injected rather than posted, because a posted `WM_KEYDOWN` is ignored by
/// anything that reads the keyboard state directly, and a shell's job is to
/// reach `Win+D` and the volume keys — both of which do.
pub fn send_chord(spec: &str) -> Result<(), String> {
    let (mods, key) =
        parse_hotkey(spec).ok_or_else(|| format!("`{spec}` is not a chord this shell understands"))?;
    let vk = vk_of(key).ok_or_else(|| format!("`{spec}` names a key that cannot be pressed"))?;
    let left: Vec<u8> = [
        (KeyModifiers::SHIFT, VK_LSHIFT.0 as u8),
        (KeyModifiers::CTRL, VK_LCONTROL.0 as u8),
        (KeyModifiers::ALT, VK_MENU.0 as u8),
        (KeyModifiers::SUPER, VK_LWIN.0 as u8),
    ]
    .iter()
    .filter(|(flag, _)| mods.contains(*flag))
    .map(|(_, vk)| *vk)
    .collect();

    unsafe {
        for m in &left {
            keybd_event(*m, 0, KEYBD_EVENT_FLAGS(0), 0);
        }
        keybd_event(vk as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(vk as u8, 0, KEYEVENTF_KEYUP, 0);
        // Released in reverse, so a chord that ends in a modifier is not left
        // held: a stuck Win key is a broken desktop.
        for m in left.iter().rev() {
            keybd_event(*m, 0, KEYEVENTF_KEYUP, 0);
        }
    }
    Ok(())
}

/// Press and release a media key, such as `VolumeUp`.
///
/// These have no printable name, so they are named here rather than folded into
/// `send_chord`, where a user typing `VolumeUp` would expect it to work.
pub fn send_media(name: &str) -> Result<(), String> {
    let vk = match name.to_ascii_lowercase().as_str() {
        "volumeup" => 0xAF,
        "volumedown" => 0xAE,
        "mute" => 0xAD,
        "playpause" => 0xB3,
        "nexttrack" => 0xB0,
        "prevtrack" => 0xB1,
        "stop" => 0xB2,
        "brightnessup" => 0x144,
        "brightnessdown" => 0x145,
        other => return Err(format!("`{other}` is not a media key")),
    };
    unsafe {
        keybd_event(vk as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(vk as u8, 0, KEYEVENTF_KEYUP, 0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sendable_key_maps_back_to_a_code() {
        for spec in ["A", "z", "5", "F1", "F12", "Space", "Enter", "Escape", "Up", "Delete"] {
            let (mods, key) = parse_hotkey(spec).unwrap_or_else(|| panic!("{spec} should parse"));
            assert_eq!(mods, KeyModifiers::NONE, "{spec}");
            assert!(vk_of(key).is_some(), "{spec} has no virtual key");
        }
    }

    #[test]
    fn a_key_with_no_code_is_refused_rather_than_guessed() {
        // Injecting the wrong key is worse than injecting none.
        let (mods, key) = parse_hotkey("Super+Space").unwrap();
        assert!(vk_of(key).is_some());
        assert!(vk_of(Key::Other).is_none(), "{mods:?} must not fall through to a wrong code");
    }

    #[test]
    fn chords_and_media_keys_parse() {
        assert!(parse_hotkey("Win+D").is_some());
        assert!(send_media("VolumeUp").is_ok());
        assert!(send_media("Hyper+Plus").is_err());
    }

    #[test]
    fn a_nonsense_chord_is_refused() {
        let e = send_chord("Hyper+Plus").unwrap_err();
        assert!(e.contains("not a chord"), "{e}");
    }

    #[test]
    fn parses_a_composed_hotkey() {
        let (mods, key) = parse_hotkey("Super+Space").unwrap();
        assert!(mods.contains(KeyModifiers::SUPER));
        assert_eq!(key, Key::Space);
    }

    #[test]
    fn parses_modifier_chains() {
        let (mods, key) = parse_hotkey("Ctrl+Shift+P").unwrap();
        assert!(mods.contains(KeyModifiers::CTRL));
        assert!(mods.contains(KeyModifiers::SHIFT));
        assert_eq!(key, Key::Char('P'));
    }

    #[test]
    fn parses_function_keys() {
        assert_eq!(parse_hotkey("Alt+F4").unwrap().1, Key::F(4));
        assert_eq!(parse_hotkey("F12").unwrap().1, Key::F(12));
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_hotkey("").is_none());
        assert!(parse_hotkey("Ctrl").is_none());
        assert!(parse_hotkey("F99").is_none());
    }

    #[test]
    fn round_trips_through_vk() {
        for spec in ["Super+Space", "Ctrl+P", "F5", "Alt+Left"] {
            let (mods, key) = parse_hotkey(spec).unwrap();
            let vk = key_to_vk(key);
            assert_ne!(vk, 0, "{spec} must map to a virtual key");
            assert_ne!(hotkey_modifiers(mods), 0);
        }
    }

    #[test]
    fn modifiers_are_always_no_repeat() {
        assert!(hotkey_modifiers(KeyModifiers::CTRL) & 0x4000 != 0);
    }

    #[test]
    fn describes_modifiers_in_a_stable_order() {
        assert_eq!(describe_modifiers(KeyModifiers::SUPER | KeyModifiers::CTRL), "Ctrl+Super");
    }
}

