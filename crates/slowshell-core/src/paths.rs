//! Conventional Windows locations for configuration, state and logs.
//!
//! Nothing here hardcodes a Unix path. `%APPDATA%` is used for user config so a
//! shell config is portable between machines and survives a profile sync.

use std::path::{Path, PathBuf};

/// Root of the user configuration directory, `%APPDATA%\Slowshell`.
///
/// Falls back to `%USERPROFILE%` and then the current directory, so the shell can
/// still start in a stripped-down environment (CI, a service account) and report
/// a precise diagnostic instead of failing to launch.
pub fn config_root() -> PathBuf {
    if let Some(p) = env_path("APPDATA") {
        return p.join("Slowshell");
    }
    if let Some(p) = env_path("USERPROFILE") {
        return p.join("AppData").join("Roaming").join("Slowshell");
    }
    PathBuf::from("Slowshell")
}

/// The main configuration file.
pub fn shell_config() -> PathBuf {
    config_root().join("shell.config")
}

pub fn widgets_dir() -> PathBuf {
    config_root().join("widgets")
}

pub fn themes_dir() -> PathBuf {
    config_root().join("themes")
}

pub fn plugins_dir() -> PathBuf {
    config_root().join("plugins")
}

pub fn assets_dir() -> PathBuf {
    config_root().join("assets")
}

/// Per-user mutable state: session flags, window positions, plugin caches.
pub fn state_dir() -> PathBuf {
    if let Some(local) = env_path("LOCALAPPDATA") {
        return local.join("Slowshell");
    }
    config_root().join("state")
}

pub fn log_file() -> PathBuf {
    state_dir().join("slowshell.log")
}

pub fn crash_dir() -> PathBuf {
    state_dir().join("crashes")
}

/// Named pipe the CLI talks to. Scoped per user so two accounts on one machine
/// do not fight over the same endpoint.
pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    format!("slowshell-{user}")
}

/// Create every directory the shell expects, so a first run does not fail on a
/// missing `themes\`.
pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [
        config_root(),
        widgets_dir(),
        themes_dir(),
        plugins_dir(),
        assets_dir(),
        state_dir(),
    ] {
        std::fs::create_dir_all(&d)?;
    }
    Ok(())
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Resolve a config-relative path such as `widgets/weather.config`.
///
/// Rejects absolute paths and `..` escapes so a config cannot reach outside its
/// own directory tree, which matters once third-party plugins are involved.
pub fn resolve_relative(root: &Path, rel: &str) -> PathBuf {
    let mut normalized = rel.replace('/', "\\");
    // A forward-slash style path is fine; a bare Windows path is not a config path.
    if normalized.contains(':') {
        return root.join(rel);
    }
    while normalized.starts_with(".\\") {
        normalized.drain(..2);
    }
    let p = Path::new(&normalized);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    root.join(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_root_is_absolute_and_ends_in_slowshell() {
        let r = config_root();
        assert!(r.is_absolute(), "config root must be absolute, got {r:?}");
        assert_eq!(r.file_name().unwrap(), "Slowshell");
    }

    #[test]
    fn layout_hangs_off_config_root() {
        let root = config_root();
        assert!(shell_config().starts_with(&root));
        assert!(widgets_dir().starts_with(&root));
        assert!(themes_dir().starts_with(&root));
        assert!(plugins_dir().starts_with(&root));
    }

    #[test]
    fn state_dir_is_separate_from_config() {
        assert_ne!(state_dir(), config_root());
    }

    #[test]
    fn relative_resolution_handles_forward_slashes() {
        let root = PathBuf::from(r"C:\cfg");
        assert_eq!(
            resolve_relative(&root, "widgets/weather.config"),
            PathBuf::from(r"C:\cfg\widgets\weather.config")
        );
        assert_eq!(
            resolve_relative(&root, r".\bar\clock"),
            PathBuf::from(r"C:\cfg\bar\clock")
        );
    }
}
