//! The currently playing track, via Global System Media Control Transport.
//!
//! # Why this works without a package identity
//!
//! Windows exposes "what is playing" through SMTC, and SMTC has two doors. The
//! per-application one, `SystemMediaTransportControls.GetForCurrentView`, needs
//! the caller to have a package identity, which a desktop program does not have.
//! The *global* one, `GlobalSystemMediaTransportControlsSessionManager`, does
//! not, and it lists whatever every other process is publishing.
//!
//! That distinction was worth checking rather than assuming: this crate's
//! `media.config` used to state that reading the current track was impossible
//! without a UWP identity. That was true of the wrong door. `cargo run -p
//! slowshell-win --example capability_probe` prints what the global manager
//! actually returns.
//!
//! # Why a thread polls instead of subscribing to events
//!
//! SMTC's `SessionsChanged` and `CurrentSessionChanged` events are delivered
//! through a COM message pump on the thread that requested them. The shell's
//! frame loop must never block on a COM call, and giving it a pump means every
//! other subsystem has to become apartment-aware first. So the watcher owns a
//! background thread that asks on an interval, and the frame loop reads a
//! snapshot. The cost lands where idle work is free, and the shell's own idle
//! budget is untouched — the frame loop does one mutex read per tick, and only
//! repaints when the snapshot actually changed.
//!
//! Polling is not a compromise made for convenience here. It is the same
//! reasoning as the clock: something has to notice that the world changed, and a
//! dedicated thread is the cheapest place to do it that does not put a COM call
//! anywhere near presentation.

/// Poll a WinRT operation to completion, with a deadline.
///
/// A macro rather than a function because `GetResults` needs `T: RuntimeType`,
/// and that trait is in a private module of `windows-core`, so a generic helper
/// cannot name its own bound. Expanding inline sidesteps it.
///
/// The deadline is not decoration: a provider that hangs must not take the
/// watcher thread with it, because a dead watcher looks exactly like a shell
/// that will never show a track again.
macro_rules! block_on {
    ($op:expr) => {{
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            // AsyncStatus::Completed is 1. The enum is an i32 newtype with no
            // derives, so the raw value is the only thing available.
            if $op.Status()?.0 == 1 {
                break $op.GetResults()?;
            }
            if std::time::Instant::now() > deadline {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }};
}


use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the shell publishes about the current media session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaSnapshot {
    /// Track title, or empty when nothing is identified as playing.
    pub title: String,
    /// Track artist.
    pub artist: String,
    /// `playing`, `paused`, `stopped` or `closed`.
    pub status: String,
    /// The publishing application's user model id, when there is one.
    pub app: String,
}

impl MediaSnapshot {
    /// Whether a track is identified and something is playing or paused.
    pub fn is_playing(&self) -> bool {
        !self.title.is_empty() && (self.status == "playing" || self.status == "paused")
    }
}

/// Shared between the watcher thread and the frame loop.
#[derive(Debug, Default)]
pub struct MediaState {
    inner: Mutex<MediaSnapshot>,
    /// Bumped on every real change, so the reader can skip an unchanged snapshot
    /// without cloning a string to find out.
    revision: AtomicBool,
}

impl MediaState {
    /// Read the current snapshot, or `None` when it has not changed since the
    /// last read.
    ///
    /// `None` is the common case and the point: a bar that repaints because a
    /// mutex was locked is a bar burning the idle budget for nothing.
    pub fn take(&self) -> Option<MediaSnapshot> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            // A poisoned lock means the watcher panicked. Reporting "unchanged"
            // leaves the last good values on screen, which is the right failure:
            // a stale track title beats a blank bar or a crashed shell.
            Err(poisoned) => poisoned.into_inner(),
        };
        if !self.revision.swap(false, Ordering::Relaxed) {
            return None;
        }
        guard.clone().into()
    }

    fn publish(&self, next: MediaSnapshot) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // An identical snapshot is not a change. This is the difference between a
        // media widget that costs nothing when the track is not moving and one
        // that repaints the bar every poll for the life of the session.
        if *guard == next {
            return;
        }
        *guard = next;
        self.revision.store(true, Ordering::Relaxed);
    }
}

/// Start watching for the current media session.
///
/// Returns immediately; the work happens on a thread of its own. The watcher
/// lives until the process exits, which for a desktop shell is the correct
/// lifetime — a bar does not get torn down while a track is playing.
pub fn watch() -> Arc<MediaState> {
    let state = Arc::new(MediaState::default());
    let worker = state.clone();
    // A failure to spawn leaves the state permanently unchanged, which the frame
    // loop reads as "no change ever" and the config shows nothing for. Not fatal:
    // a bar without a track title is a bar, just a quieter one.
    let _ = std::thread::Builder::new()
        .name("slowshell-media".into())
        .spawn(move || run(worker));
    state
}

fn run(state: Arc<MediaState>) {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    unsafe {
        // STA, because SMTC's WinRT activation expects it. A failure here leaves
        // the thread uninitialised, which WinRT tolerates but is worth not
        // pretending did not happen.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let mut first = true;
        loop {
            // A failure here means "nothing is playing", which is the right thing
            // to show and the right thing not to report.
            state.publish(read_session().unwrap_or_default());
            // The first read is immediate so a bar that starts while something is
            // already playing shows it without a two-second blank. After that,
            // slower: tracks change on the order of minutes.
            std::thread::sleep(if first {
                first = false;
                Duration::from_millis(50)
            } else {
                Duration::from_millis(2_000)
            });
        }
    }
}

/// Read the current session.
///
/// Fallsible so the `block_on!` macro can propagate a timeout; the caller
/// collapses any failure to "nothing is playing", because a shell should not care
/// loudly about a track title.
fn read_session() -> windows::core::Result<MediaSnapshot> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
        let op = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?;
        let mgr = block_on!(&op);

        let Ok(session) = mgr.GetCurrentSession() else {
            // No current session. SMTC reports this as an error rather than an
            // empty value, so it is the normal "nothing is playing" answer and
            // must not be treated as a failure.
            return Ok(MediaSnapshot::default());
        };

        let mut out = MediaSnapshot {
            status: match session.GetPlaybackInfo() {
                Ok(info) => match info.PlaybackStatus() {
                    Ok(s) => format!("{s:?}").to_lowercase(),
                    Err(_) => "unknown".to_string(),
                },
                Err(_) => "unknown".to_string(),
            },
            app: session
                .SourceAppUserModelId()
                .map(|a| a.to_string())
                .unwrap_or_default(),
            ..Default::default()
        };

        if let Ok(props_op) = session.TryGetMediaPropertiesAsync() {
            let props = block_on!(&props_op);
            out.title = props.Title().map(|t| t.to_string()).unwrap_or_default();
            out.artist = props.Artist().map(|a| a.to_string()).unwrap_or_default();
        }
        Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untouched_state_reports_no_change() {
        // The property the idle budget depends on: reading a watcher that has
        // not published anything must cost nothing and mean nothing.
        let s = MediaState::default();
        assert!(s.take().is_none());
        assert!(s.take().is_none());
    }

    #[test]
    fn a_first_publication_is_reported_once() {
        let s = MediaState::default();
        s.publish(MediaSnapshot {
            title: "Blue Monday".into(),
            artist: "New Order".into(),
            status: "playing".into(),
            app: String::new(),
        });
        let first = s.take().expect("the first read must see the change");
        assert_eq!(first.title, "Blue Monday");
        assert!(first.is_playing());
        // And only once: the frame loop must not repaint every tick for a track
        // that has not moved.
        assert!(s.take().is_none(), "an unchanged snapshot must read as no change");
    }

    #[test]
    fn publishing_the_same_track_twice_is_not_a_change() {
        // This is the difference between a media widget that costs nothing and
        // one that repaints the bar twice a second forever.
        let s = MediaState::default();
        let track = MediaSnapshot {
            title: "Ceremony".into(),
            status: "playing".into(),
            ..Default::default()
        };
        s.publish(track.clone());
        assert!(s.take().is_some());
        s.publish(track);
        assert!(s.take().is_none(), "the same track must not count as a change");
    }

    #[test]
    fn a_snapshot_with_no_title_is_not_playing() {
        // SMTC reports a session with an unidentifiable track. Showing the
        // controls is right; showing a track title would be inventing one.
        let s = MediaSnapshot {
            status: "playing".into(),
            ..Default::default()
        };
        assert!(!s.is_playing());
    }

    #[test]
    fn a_paused_track_still_counts_as_playing() {
        // Paused is what the user wants on their bar: they paused it on purpose.
        let s = MediaSnapshot {
            title: "Windowlicker".into(),
            status: "paused".into(),
            ..Default::default()
        };
        assert!(s.is_playing());
    }
}
