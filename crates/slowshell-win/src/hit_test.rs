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

use crate::window::WindowState;

thread_local! {
    /// Interactive rectangles for the window currently being hit-tested, in
    /// logical pixels relative to the window origin.
    static REGIONS: RefCell<Vec<HitRegion>> = const { RefCell::new(Vec::new()) };
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

/// Replace the interactive regions for the frame about to be drawn.
///
/// Called by the layout pass. `regions` must already be in logical pixels.
pub fn set_regions(regions: Vec<HitRegion>) {
    REGIONS.with(|r| *r.borrow_mut() = regions);
}

pub fn clear_regions() {
    REGIONS.with(|r| r.borrow_mut().clear());
}

pub fn regions() -> Vec<HitRegion> {
    REGIONS.with(|r| r.borrow().clone())
}

/// Alias so callers can refer to the hit-test surface as a single concept.
pub type HitTest = Vec<HitRegion>;

/// The topmost interactive region containing a point, or `None`.
///
/// The search happens inside the `RefCell` borrow and returns a plain value, so no
/// reference to the region escapes the thread-local.
fn topmost_at(x: f32, y: f32) -> Option<HitRegion> {
    REGIONS.with(|g| {
        let regions = g.borrow();
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

/// Whether the point lands on an interactive region.
pub fn hit_test(state: &WindowState, x: f32, y: f32) -> bool {
    if state.click_through {
        return false;
    }
    topmost_at(x, y).is_some()
}

/// The element id under a point, if any. Used to route a click.
pub fn region_at(x: f32, y: f32) -> Option<u32> {
    topmost_at(x, y).map(|r| r.id)
}

/// Mark a region as visual-only so clicks fall through it.
pub fn make_passive(r: &mut HitRegion) {
    r.interactive = false;
}

#[cfg(test)]
mod tests {
    use super::*;

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
        set_regions(vec![HitRegion::new(0.0, 0.0, 10.0, 10.0, 1)]);
        assert!(hit_test(&state(), 5.0, 5.0));
        assert!(!hit_test(&state(), 500.0, 5.0), "outside every region");
        clear_regions();
    }

    #[test]
    fn no_regions_means_fully_pass_through() {
        clear_regions();
        assert!(!hit_test(&state(), 0.0, 0.0));
    }

    #[test]
    fn passive_regions_do_not_capture() {
        let mut r = HitRegion::new(0.0, 0.0, 50.0, 50.0, 1);
        make_passive(&mut r);
        set_regions(vec![r]);
        assert!(!hit_test(&state(), 10.0, 10.0));
        clear_regions();
    }

    #[test]
    fn a_higher_layer_wins_an_overlap() {
        let mut top = HitRegion::new(0.0, 0.0, 50.0, 50.0, 2);
        top.layer = 10;
        let bottom = HitRegion::new(0.0, 0.0, 100.0, 100.0, 1);
        set_regions(vec![bottom, top]);
        assert_eq!(region_at(10.0, 10.0), Some(2));
        // Outside the top region the bottom one still answers.
        assert_eq!(region_at(80.0, 80.0), Some(1));
        clear_regions();
    }

    #[test]
    fn click_through_overrides_regions() {
        set_regions(vec![HitRegion::new(0.0, 0.0, 50.0, 50.0, 1)]);
        let mut s = state();
        s.click_through = true;
        assert!(!hit_test(&s, 10.0, 10.0));
        clear_regions();
    }
}
