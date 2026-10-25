//! Per-pixel click-through.
//!
//! A desktop bar covers the whole width of a screen but is mostly empty. If those
//! empty pixels swallowed clicks, a bar would make the desktop underneath unusable.
//! `WM_NCHITTEST` returning `HTTRANSPARENT` lets Windows continue its hit test
//! past this window to the next one in the z-order.
//!
//! The interactive regions are published by the layout pass each frame, so this
//! stays a cheap rectangle test on the message path.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::window::WindowState;

thread_local! {
    /// Interactive rectangles for each window, keyed by the window's id, in
    /// logical pixels relative to that window's origin.
    ///
    /// Per window, not one list: with one list the last window painted owned
    /// every click, so a shell with a bar and a few drawers routed clicks on the
    /// bar through a drawer's buttons and nothing on the bar worked.
    static REGIONS: RefCell<HashMap<isize, Vec<HitRegion>>> = RefCell::new(HashMap::new());
}

/// One interactive area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitRegion {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// The element that owns the region, so a click can be routed back to it.
    pub id: u32,
    /// A region with a higher layer wins, which is how a popup above a panel
    /// captures the click even if they overlap.
    pub layer: u16,
    /// Whether the region swallows clicks at all. A purely visual element sets
    /// `false` so the desktop stays usable through it.
    pub interactive: bool,
}

impl HitRegion {
    pub fn new(x: f32, y: f32, w: f32, h: f32, id: u32) -> HitRegion {
        HitRegion { x, y, w, h, id, layer: 0, interactive: true }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Replace `window`'s interactive regions for the frame about to be drawn.
///
/// Called by the layout pass. `regions` must already be in logical pixels.
pub fn set_regions(window: isize, regions: Vec<HitRegion>) {
    REGIONS.with(|r| {
        r.borrow_mut().insert(window, regions);
    });
}

/// Forget `window`'s regions, for a window that is going away.
pub fn clear_regions(window: isize) {
    REGIONS.with(|r| {
        r.borrow_mut().remove(&window);
    });
}

pub fn regions(window: isize) -> Vec<HitRegion> {
    REGIONS.with(|r| r.borrow().get(&window).cloned().unwrap_or_default())
}

/// Alias so callers can refer to the hit-test surface as a single concept.
pub type HitTest = Vec<HitRegion>;

/// The topmost interactive region containing a point, or `None`.
///
/// The search happens inside the `RefCell` borrow and returns a plain value, so no
/// reference to the region escapes the thread-local.
fn topmost_at(window: isize, x: f32, y: f32) -> Option<HitRegion> {
    REGIONS.with(|g| {
        let all = g.borrow();
        let regions = all.get(&window)?;
        let mut best: Option<HitRegion> = None;
        for r in regions.iter() {
            if !r.interactive || !r.contains(x, y) {
                continue;
            }
            // Highest layer wins; ties go to the later element, which is the one
            // drawn on top.
            if best.as_ref().is_none_or(|b| r.layer >= b.layer) {
                best = Some(*r);
            }
        }
        best
    })
}

/// Whether the point, in logical pixels relative to `window`, lands on one of
/// its interactive regions.
pub fn hit_test(state: &WindowState, window: isize, x: f32, y: f32) -> bool {
    if state.click_through {
        return false;
    }
    topmost_at(window, x, y).is_some()
}

/// The element id under a point in `window`, in logical pixels relative to it.
/// Used to route a click.
pub fn region_at(window: isize, x: f32, y: f32) -> Option<u32> {
    topmost_at(window, x, y).map(|r| r.id)
}

/// Mark a region as visual-only so clicks fall through it.
pub fn make_passive(r: &mut HitRegion) {
    r.interactive = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: isize = 7;

    fn state() -> WindowState {
        WindowState::for_tests()
    }

    #[test]
    fn containment_is_half_open_so_adjacent_regions_do_not_overlap() {
        let r = HitRegion::new(0.0, 0.0, 10.0, 10.0, 1);
        assert!(r.contains(0.0, 0.0));
        assert!(r.contains(9.9, 9.9));
        assert!(!r.contains(10.0, 5.0));
        assert!(!r.contains(5.0, 10.0));
        assert!(!r.contains(-0.1, 5.0));
    }

    #[test]
    fn empty_space_is_pass_through() {
        set_regions(W, vec![HitRegion::new(0.0, 0.0, 10.0, 10.0, 1)]);
        assert!(hit_test(&state(), W, 5.0, 5.0));
        assert!(!hit_test(&state(), W, 500.0, 5.0), "outside every region");
        clear_regions(W);
    }

    #[test]
    fn no_regions_means_fully_pass_through() {
        clear_regions(W);
        assert!(!hit_test(&state(), W, 0.0, 0.0));
    }

    #[test]
    fn passive_regions_do_not_capture() {
        let mut r = HitRegion::new(0.0, 0.0, 50.0, 50.0, 1);
        make_passive(&mut r);
        set_regions(W, vec![r]);
        assert!(!hit_test(&state(), W, 10.0, 10.0));
        clear_regions(W);
    }

    #[test]
    fn a_higher_layer_wins_an_overlap() {
        let mut top = HitRegion::new(0.0, 0.0, 50.0, 50.0, 2);
        top.layer = 10;
        let bottom = HitRegion::new(0.0, 0.0, 100.0, 100.0, 1);
        set_regions(W, vec![bottom, top]);
        assert_eq!(region_at(W, 10.0, 10.0), Some(2));
        // Outside the top region the bottom one still answers.
        assert_eq!(region_at(W, 80.0, 80.0), Some(1));
        clear_regions(W);
    }

    #[test]
    fn each_window_answers_only_for_its_own_regions() {
        set_regions(W, vec![HitRegion::new(0.0, 0.0, 50.0, 50.0, 1)]);
        set_regions(W + 1, vec![HitRegion::new(0.0, 0.0, 50.0, 50.0, 2)]);
        assert_eq!(region_at(W, 10.0, 10.0), Some(1));
        assert_eq!(region_at(W + 1, 10.0, 10.0), Some(2));
        clear_regions(W + 1);
        assert_eq!(region_at(W + 1, 10.0, 10.0), None);
        clear_regions(W);
    }

    #[test]
    fn click_through_overrides_regions() {
        set_regions(W, vec![HitRegion::new(0.0, 0.0, 50.0, 50.0, 1)]);
        let mut s = state();
        s.click_through = true;
        assert!(!hit_test(&s, W, 10.0, 10.0));
        clear_regions(W);
    }
}
