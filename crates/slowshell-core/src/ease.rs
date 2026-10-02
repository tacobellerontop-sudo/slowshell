//! Easing curves.
//!
//! # Why the shape matters more than the duration
//!
//! A thing that appears in 200 ms with linear timing reads as mechanical, because
//! nothing in the physical world starts and stops at constant speed. Real motion
//! spends most of its time near rest and its middle travelling fast. An easing
//! curve is the cheapest way to get that, and it is a few dozen lines of
//! arithmetic against a GPU that would otherwise be idle.
//!
//! `ease_out_cubic` is the default: most of the movement happens early and the
//! last few pixels settle, which is what makes something feel *responsive* rather
//! than fast. `ease_out_back` overshoots slightly and is the one to reach for when
//! something should feel physical.
//!
//! Curves are defined on 0..=1 and are pure functions of it, so they are testable
//! without a window, a clock, or a display — which matters, because "does this feel
//! right" is otherwise only answerable by eye, and by eye it is answered wrongly
//! on a machine with a 60 Hz panel and correctly on one with 144.

/// How a value moves from its start to its end over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    /// Constant speed. Rarely what you want for something appearing.
    Linear,
    /// Fast start, gentle settle. The default.
    OutCubic,
    /// Gentle start and end, fast in the middle. For something moving *within*
    /// view rather than appearing.
    InOutCubic,
    /// Overshoots and comes back. Physical, and the most noticeable.
    OutBack,
    /// Very fast start, long tail. For something that should feel instant.
    OutExpo,
}

impl Ease {
    /// Parse a config name. Unknown names fall back rather than failing: an easing
    /// typo should produce the default animation, not a shell that will not start.
    pub fn parse(name: &str) -> Ease {
        match name {
            "linear" => Ease::Linear,
            "outCubic" | "easeOut" => Ease::OutCubic,
            "inOutCubic" | "easeInOut" => Ease::InOutCubic,
            "outBack" | "easeOutBack" => Ease::OutBack,
            "outExpo" | "easeOutExpo" => Ease::OutExpo,
            _ => Ease::OutCubic,
        }
    }

    /// The config name, for diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Ease::Linear => "linear",
            Ease::OutCubic => "outCubic",
            Ease::InOutCubic => "inOutCubic",
            Ease::OutBack => "outBack",
            Ease::OutExpo => "outExpo",
        }
    }

    /// The curve's value at `t`, where `t` is progress through the animation.
    ///
    /// `t` is clamped to 0..=1, so an animation that is stepped slightly past its
    /// end lands on exactly 1 rather than extrapolating past the target and
    /// leaving a panel a few pixels too wide forever.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::OutCubic => 1.0 - (1.0 - t).powi(3),
            Ease::InOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Ease::OutBack => {
                // c1 and c3 from the reference curve. c1 sets how far it overshoots
                // and c3 pulls it back so the value still ends exactly at 1.
                const C1: f32 = 1.70158;
                const C3: f32 = C1 + 1.0;
                1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
            }
            Ease::OutExpo => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - (-10.0 * t).exp2()
                }
            }
        }
    }
}

impl std::fmt::Display for Ease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A value moving between two points over a fixed duration.
///
/// Deliberately not a general animation engine. It is one channel, it is stepped
/// from the frame loop, and it is the thing the reveal panels and any future
/// transition both need. A general engine would be a larger thing to get right and
/// a smaller thing to use.
#[derive(Debug, Clone, Copy)]
pub struct Tween {
    from: f32,
    to: f32,
    /// Progress through the animation, 0..=1, before easing is applied.
    pub progress: f32,
    ease: Ease,
    /// How long the whole movement takes, in seconds.
    ///
    /// Per tween rather than a constant, because `revealDuration` is a config
    /// setting and a setting that is read and then ignored is worse than one that
    /// does not exist.
    duration: f32,
}

impl Tween {
    /// A tween already at its target, so the first frame draws the final value
    /// rather than animating from nowhere.
    pub fn settled(value: f32) -> Tween {
        Tween {
            from: value,
            to: value,
            progress: 1.0,
            ease: Ease::OutCubic,
            duration: Tween::DURATION,
        }
    }

    /// Start a tween from `from` to `to`, taking `duration` seconds.
    pub fn new(from: f32, to: f32, ease: Ease, duration: f32) -> Tween {
        Tween {
            from,
            to,
            progress: 0.0,
            ease,
            // Clamped away from zero: a zero duration divides by zero on every
            // step, progress becomes NaN, and `NaN >= 1.0` is false — so the tween
            // animates forever and the frame loop never sleeps again.
            duration: duration.max(0.001),
        }
    }

    /// Change how long this tween takes, keeping its current progress.
    pub fn with_duration(&mut self, duration: f32) {
        self.duration = duration.max(0.001);
    }

    /// Retarget without jumping. The current eased value becomes the new start, so
    /// reversing mid-flight is smooth rather than a visible snap back to `from`.
    ///
    /// This is what makes a hover menu feel right: pulling the pointer away and
    /// bringing it back quickly must not restart the expansion from nothing.
    pub fn retarget(&mut self, to: f32, ease: Ease) {
        let current = self.value();
        // Already there: leave `progress` alone so a settled tween stays settled.
        if (current - to).abs() < f32::EPSILON {
            self.to = to;
            return;
        }
        self.from = current;
        self.to = to;
        self.progress = 0.0;
        self.ease = ease;
    }

    /// Whether the tween has arrived and has nothing left to do.
    pub fn is_settled(&self) -> bool {
        self.progress >= 1.0
    }

    /// The current value, eased.
    pub fn value(&self) -> f32 {
        self.from + (self.to - self.from) * self.ease.apply(self.progress)
    }

    /// The destination, regardless of progress.
    pub fn target(&self) -> f32 {
        self.to
    }

    /// Advance by `dt` seconds, returning whether anything changed.
    ///
    /// Returns a bool rather than `()` because the frame loop uses it to decide
    /// whether it must keep repainting: a tween that has arrived must not hold the
    /// loop awake, which is the whole reason the idle budget survives having
    /// animations at all.
    pub fn advance(&mut self, dt: f32) -> bool {
        if self.progress >= 1.0 {
            return false;
        }
        self.progress = (self.progress + dt / self.duration).min(1.0);
        true
    }

    /// The animation's length, in seconds.
    pub const DURATION: f32 = 0.18;
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Ease; 5] = [
        Ease::Linear,
        Ease::OutCubic,
        Ease::InOutCubic,
        Ease::OutBack,
        Ease::OutExpo,
    ];

    #[test]
    fn every_curve_starts_at_zero_and_ends_at_one() {
        // The two values that must be exact. A curve that ends at 0.999 leaves a
        // panel a fraction of a pixel from where it belongs, forever, because
        // nothing ever nudges it the last step.
        for e in ALL {
            assert_eq!(e.apply(0.0), 0.0, "{} starts at {}", e.name(), e.apply(0.0));
            assert_eq!(e.apply(1.0), 1.0, "{} ends at {}", e.name(), e.apply(1.0));
        }
    }

    #[test]
    fn progress_outside_the_range_is_clamped_not_extrapolated() {
        // The frame loop steps by wall-clock time, so it can overshoot the end by
        // a fraction of a frame. Extrapolating would push the value past the
        // target and it would never come back.
        for e in ALL {
            assert_eq!(e.apply(-0.5), 0.0, "{} went below 0", e.name());
            assert_eq!(e.apply(1.5), 1.0, "{} went above 1", e.name());
        }
    }

    #[test]
    fn the_non_overshooting_curves_never_go_backwards() {
        // A non-monotonic curve makes something appear to hesitate or stutter
        // mid-flight, which reads as lag rather than as animation.
        for e in [Ease::Linear, Ease::OutCubic, Ease::InOutCubic, Ease::OutExpo] {
            let mut last = 0.0;
            for i in 0..=100 {
                let v = e.apply(i as f32 / 100.0);
                assert!(
                    v >= last,
                    "{} went backwards at {i}: {} then {}",
                    e.name(),
                    last,
                    v
                );
                last = v;
            }
        }
    }

    #[test]
    fn out_back_overshoots_on_purpose() {
        // The one curve that is meant to exceed 1 in the middle, and that is the
        // whole reason it exists. If it stops overshooting the name is a lie.
        let peak = (0..=100)
            .map(|i| Ease::OutBack.apply(i as f32 / 100.0))
            .fold(f32::MIN, f32::max);
        assert!(peak > 1.0, "outBack never overshoots, peak was {peak}");
        assert!(peak < 1.2, "outBack overshoots far too much: {peak}");
    }

    #[test]
    fn the_default_curve_moves_more_early_than_late() {
        // The property that makes something feel responsive: at a quarter of the
        // way through, ease-out has already covered more than a quarter.
        assert!(Ease::OutCubic.apply(0.25) > 0.25);
        assert!(Ease::OutCubic.apply(0.25) > Ease::Linear.apply(0.25));
        // And ease-in-out is the opposite, which is why it is not the default.
        assert!(Ease::InOutCubic.apply(0.25) < 0.25);
    }

    #[test]
    fn an_unknown_easing_name_falls_back_rather_than_failing() {
        // A typo in `revealEase` must not stop the shell starting. Falling back to
        // the default animation is invisible; refusing to launch is not.
        assert_eq!(Ease::parse("nonsense"), Ease::OutCubic);
        assert_eq!(Ease::parse(""), Ease::OutCubic);
        assert_eq!(Ease::parse("outBack"), Ease::OutBack);
        for e in ALL {
            assert_eq!(Ease::parse(e.name()), e, "{e} did not round-trip");
        }
    }

    #[test]
    fn a_settled_tween_reports_itself_settled() {
        let mut t = Tween::settled(42.0);
        assert!(t.is_settled());
        assert_eq!(t.value(), 42.0);
        // And advancing a settled tween changes nothing, so a loop that keeps
        // calling advance on a finished animation cannot hold itself awake.
        assert!(!t.advance(0.016));
    }

    #[test]
    fn a_tween_arrives_at_its_target_and_stops() {
        let mut t = Tween::new(0.0, 100.0, Ease::OutCubic, Tween::DURATION);
        let mut steps = 0;
        while t.advance(0.016) {
            steps += 1;
            assert!(steps < 100, "a tween never arrived");
        }
        assert!(t.is_settled());
        assert_eq!(t.value(), 100.0);
        // 180 ms at 60 Hz is about 11 frames, so a tween that takes many more than
        // that is reporting time wrong.
        assert!(steps < 30, "took {steps} frames for a 180ms tween");
    }

    #[test]
    fn reversing_mid_flight_does_not_snap_back_to_the_start() {
        // The behaviour a hover menu depends on. If retargeting restarted from
        // `from`, pulling the pointer away and back would make the panel jump to
        // collapsed and then re-expand, which reads as a glitch.
        let mut t = Tween::new(0.0, 100.0, Ease::OutCubic, Tween::DURATION);
        t.advance(0.09); // halfway-ish
        let mid = t.value();
        assert!(mid > 5.0 && mid < 95.0, "not actually mid-flight: {mid}");
        t.retarget(0.0, Ease::OutCubic);
        let after = t.value();
        assert!(
            (after - mid).abs() < 1.0,
            "retarget jumped from {mid} to {after}"
        );
        assert_eq!(t.target(), 0.0);
    }

    #[test]
    fn retargeting_to_where_it_already_is_leaves_it_settled() {
        // Re-hovering a settled panel must not start it animating from nowhere,
        // which is what would happen if `progress` were reset unconditionally.
        let mut t = Tween::settled(100.0);
        t.retarget(100.0, Ease::OutBack);
        assert!(t.is_settled(), "retarget to the same value un-settled it");
        assert_eq!(t.value(), 100.0);
    }

    #[test]
    fn a_tween_from_a_to_b_passes_through_the_middle() {
        // Guards against an inverted `from`/`to`, which is the one arithmetic
        // mistake here and which renders as a panel that shrinks when it opens.
        let mut t = Tween::new(8.0, 320.0, Ease::Linear, Tween::DURATION);
        let mut seen_below = false;
        let mut seen_above = false;
        while t.advance(0.016) {
            let v = t.value();
            if v < 8.0 {
                seen_below = true;
            }
            if v > 320.0 {
                seen_above = true;
            }
        }
        assert!(!seen_below && !seen_above, "the tween left its own range");
        assert_eq!(t.value(), 320.0);
    }
}
