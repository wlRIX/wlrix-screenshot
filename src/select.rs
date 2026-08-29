// SPDX-License-Identifier: GPL-3.0-or-later
//! The selection rectangle, and everything that can be done to it.
//!
//! Pure state: this takes pointer positions and key presses and gives back whether anything
//! changed, so the whole interaction can be exercised without a compositor. [`crate::ui`] owns
//! turning Wayland events into these calls and redrawing when one reports a change. That is the
//! split `wlrix-desktop/src/select.rs` draws, for the same reason -- the interesting rules here
//! are geometry, and geometry does not need a screen to be tested on.
//!
//! Coordinates are **global**: the desktop is one plane spanning every monitor, and this never
//! learns there is more than one. See [`crate::shot`].
//!
//! ## The rules
//!
//! - Pressing bare desktop starts a new band. Dragging up and to the left is as valid as down
//!   and to the right, so the rectangle is normalized as it goes.
//! - Pressing a handle resizes from the opposite corner or edge, which is the one thing that
//!   makes a selection *adjustable* rather than something to redo.
//! - Pressing inside moves the whole rectangle.
//! - Arrow keys nudge, and with Shift they nudge by ten -- a keyboard has to be able to reach
//!   the last pixel, since a mouse cannot reliably.
//! - Everything is clamped to the desktop. A selection off the edge would crop to black, and
//!   the user would not find out until they looked at the file.

use wlrix_ui::canvas::Rect;

/// How far from a handle's center still counts as grabbing it.
///
/// Larger than the handle is drawn: a target the size of its own artwork is a target most
/// people miss, and the cost of being generous is only that the outer few pixels of a small
/// selection resize instead of moving.
pub const HANDLE_GRAB: i32 = 11;
/// How far the pointer must travel after a press before a new band exists at all.
///
/// Without it a click with a shaky hand leaves a two-pixel selection, and the user has to
/// press Escape and start over. Same reasoning as `wlrix-desktop`'s icon drag threshold.
pub const DRAG_THRESHOLD: i32 = 3;
/// How far Shift+arrow moves, against one pixel for a bare arrow.
pub const NUDGE_FAST: i32 = 10;

/// A point on the desktop, in global coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Which of the eight handles, named by the corner or edge it sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Handle {
    /// Every handle, in the order they are drawn and hit-tested.
    ///
    /// Corners first, so that where a corner and an edge handle overlap -- which they do on a
    /// selection narrower than two grab radii -- the corner wins. Resizing a thin selection by
    /// its corner keeps both axes reachable; by its edge, one of them is stuck.
    pub const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::TopRight,
        Handle::BottomLeft,
        Handle::BottomRight,
        Handle::Top,
        Handle::Bottom,
        Handle::Left,
        Handle::Right,
    ];

    /// Where this handle sits on a rectangle.
    pub fn centre(self, rect: Rect) -> Point {
        let (left, top) = (rect.left(), rect.top());
        // The last pixel *inside* the rectangle, not the exclusive edge -- a handle drawn one
        // pixel outside the selection reads as belonging to whatever is next to it.
        let (right, bottom) = (rect.right() - 1, rect.bottom() - 1);
        let (mid_x, mid_y) = (left + rect.w / 2, top + rect.h / 2);
        match self {
            Handle::TopLeft => Point::new(left, top),
            Handle::Top => Point::new(mid_x, top),
            Handle::TopRight => Point::new(right, top),
            Handle::Left => Point::new(left, mid_y),
            Handle::Right => Point::new(right, mid_y),
            Handle::BottomLeft => Point::new(left, bottom),
            Handle::Bottom => Point::new(mid_x, bottom),
            Handle::BottomRight => Point::new(right, bottom),
        }
    }

    /// Whether this handle moves the left, right, top or bottom edge.
    fn edges(self) -> (bool, bool, bool, bool) {
        match self {
            Handle::TopLeft => (true, false, true, false),
            Handle::Top => (false, false, true, false),
            Handle::TopRight => (false, true, true, false),
            Handle::Left => (true, false, false, false),
            Handle::Right => (false, true, false, false),
            Handle::BottomLeft => (true, false, false, true),
            Handle::Bottom => (false, false, false, true),
            Handle::BottomRight => (false, true, false, true),
        }
    }
}

/// What is under the pointer, which decides both the cursor and what a press would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grip {
    /// A handle: pressing resizes.
    Handle(Handle),
    /// Inside the selection: pressing moves the whole thing.
    Inside,
    /// Bare desktop: pressing starts a new band.
    Outside,
}

/// What a press started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    /// Pulling out a new rectangle from `anchor`.
    ///
    /// `live` is false until the pointer has traveled [`DRAG_THRESHOLD`], which is what keeps
    /// a click from replacing a good selection with a two-pixel one.
    Band { anchor: Point, live: bool },
    /// Resizing, with the opposite corner or edge held still.
    Resize { handle: Handle, fixed: Rect },
    /// Moving the whole rectangle. `grab` is where in the rectangle the press landed, so it
    /// travels under the pointer rather than jumping its top-left corner there.
    Move { grab: Point, size: (i32, i32) },
}

/// The selection, and any drag in progress.
pub struct Selection {
    /// The desktop. Everything is clamped to this.
    bounds: Rect,
    /// `None` until the user has pulled something out, which is what the overlay shows its
    /// "click and drag" prompt for.
    rect: Option<Rect>,
    drag: Option<Drag>,
}

impl Selection {
    /// An empty selection over `bounds`.
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            rect: None,
            drag: None,
        }
    }

    /// A selection that starts out set -- what `--all` and `--select` produce.
    ///
    /// Clamped, because `--select` comes from the compositor describing a window that may hang
    /// off the edge of its monitor, and a rectangle outside the desktop would crop to black.
    pub fn preset(bounds: Rect, rect: Rect) -> Self {
        let clamped = clamp(normalize(rect), bounds);
        Self {
            bounds,
            rect: (clamped.w > 0 && clamped.h > 0).then_some(clamped),
            drag: None,
        }
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// What is selected, if anything.
    pub fn rect(&self) -> Option<Rect> {
        self.rect
    }

    /// Whether a drag is in progress, so the overlay can hide the button bar while one is.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// What is under `at`, which decides the cursor and what a press there would do.
    pub fn grip_at(&self, at: Point) -> Grip {
        let Some(rect) = self.rect else {
            return Grip::Outside;
        };
        let grab = grab_radius(rect);
        for handle in Handle::ALL {
            let centre = handle.centre(rect);
            if (at.x - centre.x).abs() <= grab && (at.y - centre.y).abs() <= grab {
                return Grip::Handle(handle);
            }
        }
        if rect.contains(at.x, at.y) {
            return Grip::Inside;
        }
        Grip::Outside
    }

    /// Begin a drag. Returns whether anything changed.
    pub fn press(&mut self, at: Point) -> bool {
        match (self.grip_at(at), self.rect) {
            (Grip::Handle(handle), Some(rect)) => {
                self.drag = Some(Drag::Resize {
                    handle,
                    fixed: rect,
                });
                false
            }
            (Grip::Inside, Some(rect)) => {
                self.drag = Some(Drag::Move {
                    grab: Point::new(at.x - rect.x, at.y - rect.y),
                    size: (rect.w, rect.h),
                });
                false
            }
            _ => {
                // A new band. The old selection stays on screen until the threshold is passed,
                // so a stray click on bare desktop does not blank it.
                self.drag = Some(Drag::Band {
                    anchor: at,
                    live: false,
                });
                false
            }
        }
    }

    /// The pointer moved. Returns whether the selection changed.
    pub fn motion(&mut self, at: Point) -> bool {
        let Some(drag) = self.drag else {
            return false;
        };
        match drag {
            Drag::Band { anchor, live } => {
                let far_enough = (at.x - anchor.x).abs() >= DRAG_THRESHOLD
                    || (at.y - anchor.y).abs() >= DRAG_THRESHOLD;
                if !live && !far_enough {
                    return false;
                }
                self.drag = Some(Drag::Band { anchor, live: true });
                // Normalized as it goes: dragging up and to the left is as valid as down and
                // to the right, and the rest of the program never sees a negative width.
                let rect = normalize(Rect::new(
                    anchor.x,
                    anchor.y,
                    at.x - anchor.x,
                    at.y - anchor.y,
                ));
                self.set(rect)
            }
            Drag::Resize { handle, fixed } => {
                let (left, right, top, bottom) = handle.edges();
                // **Inclusive** corners, and that is the whole subtlety here. A handle sits on
                // a pixel, not on the exclusive edge between two, so a drag has to be resolved
                // in the same terms the handle is placed in. Mixing them puts the fixed edge
                // one pixel out -- invisible until a handle is dragged *past* the opposite
                // edge, when the rectangle that comes back is a pixel short on both sides.
                let mut x0 = fixed.left();
                let mut y0 = fixed.top();
                let mut x1 = fixed.right() - 1;
                let mut y1 = fixed.bottom() - 1;
                if left {
                    x0 = at.x;
                }
                if right {
                    x1 = at.x;
                }
                if top {
                    y0 = at.y;
                }
                if bottom {
                    y1 = at.y;
                }
                // Dragging a handle past the opposite edge flips the rectangle rather than
                // collapsing it, which is what every other selection in the world does. With
                // inclusive corners that is just a sort, and the anchor pixel stays selected
                // either way round.
                let (x0, x1) = (x0.min(x1), x0.max(x1));
                let (y0, y1) = (y0.min(y1), y0.max(y1));
                self.set(Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
            }
            Drag::Move { grab, size } => {
                let moved = Rect::new(at.x - grab.x, at.y - grab.y, size.0, size.1);
                // Slid back inside rather than clipped: a move must not also resize, or a
                // selection dragged against the edge shrinks and cannot be got back.
                self.set(slide_into(moved, self.bounds))
            }
        }
    }

    /// The button came up. Returns whether the selection changed.
    ///
    /// A band that never passed the threshold leaves the previous selection alone -- see
    /// [`DRAG_THRESHOLD`].
    pub fn release(&mut self) -> bool {
        self.drag = None;
        false
    }

    /// Move or resize by the keyboard. `resize` grows from the bottom-right rather than moving.
    pub fn nudge(&mut self, dx: i32, dy: i32, resize: bool) -> bool {
        let Some(rect) = self.rect else {
            return false;
        };
        let moved = if resize {
            Rect::new(rect.x, rect.y, rect.w + dx, rect.h + dy)
        } else {
            // Slid, not clamped, for the same reason a pointer move is.
            slide_into(
                Rect::new(rect.x + dx, rect.y + dy, rect.w, rect.h),
                self.bounds,
            )
        };
        self.set(normalize(moved))
    }

    /// Select the whole desktop.
    pub fn select_all(&mut self) -> bool {
        self.set(self.bounds)
    }

    /// Throw the selection away.
    pub fn clear(&mut self) -> bool {
        self.drag = None;
        let changed = self.rect.is_some();
        self.rect = None;
        changed
    }

    /// Clamp and store, reporting whether it moved.
    fn set(&mut self, rect: Rect) -> bool {
        let clamped = clamp(rect, self.bounds);
        let next = (clamped.w > 0 && clamped.h > 0).then_some(clamped);
        if next == self.rect {
            return false;
        }
        self.rect = next;
        true
    }
}

/// How far from a handle's centre counts as grabbing it, for a selection this size.
///
/// [`HANDLE_GRAB`] on anything roomy, and less on a small one. Eight fixed-size grab areas on a
/// 40-pixel selection leave a sixteen-pixel square in the middle that means "move me", and a
/// user who cannot find it concludes the selection cannot be moved at all. Shrinking the grab
/// area instead keeps every gesture reachable at every size.
pub fn grab_radius(rect: Rect) -> i32 {
    HANDLE_GRAB.min(rect.w / 6).min(rect.h / 6).max(2)
}

/// Make width and height positive, keeping the same two corners.
fn normalize(rect: Rect) -> Rect {
    let (x, w) = if rect.w < 0 {
        (rect.x + rect.w, -rect.w)
    } else {
        (rect.x, rect.w)
    };
    let (y, h) = if rect.h < 0 {
        (rect.y + rect.h, -rect.h)
    } else {
        (rect.y, rect.h)
    };
    Rect::new(x, y, w, h)
}

/// Clip a rectangle to the desktop.
fn clamp(rect: Rect, bounds: Rect) -> Rect {
    rect.intersect(bounds)
}

/// Slide a rectangle back inside the desktop **without changing its size**.
///
/// The difference from [`clamp`] is the whole reason both exist: clipping a moved selection
/// against the edge would shrink it, and once shrunk there is no way to get the size back
/// except by starting over. One larger than the desktop is clipped, since there is nowhere
/// left to slide it to.
fn slide_into(rect: Rect, bounds: Rect) -> Rect {
    if rect.w > bounds.w || rect.h > bounds.h {
        return clamp(rect, bounds);
    }
    let x = rect.x.clamp(bounds.left(), bounds.right() - rect.w);
    let y = rect.y.clamp(bounds.top(), bounds.bottom() - rect.h);
    Rect::new(x, y, rect.w, rect.h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop() -> Rect {
        Rect::new(0, 0, 200, 100)
    }

    fn dragged(from: Point, to: Point) -> Selection {
        let mut selection = Selection::new(desktop());
        selection.press(from);
        selection.motion(to);
        selection.release();
        selection
    }

    #[test]
    fn nothing_is_selected_to_begin_with() {
        assert_eq!(Selection::new(desktop()).rect(), None);
    }

    #[test]
    fn a_band_selects_what_it_covers() {
        let selection = dragged(Point::new(10, 20), Point::new(60, 50));
        assert_eq!(selection.rect(), Some(Rect::new(10, 20, 50, 30)));
    }

    /// Dragging up and to the left is as valid as down and to the right, and nothing above
    /// this ever sees a negative width.
    #[test]
    fn a_band_dragged_backwards_is_the_same_rectangle() {
        let forward = dragged(Point::new(10, 20), Point::new(60, 50));
        let backward = dragged(Point::new(60, 50), Point::new(10, 20));
        assert_eq!(forward.rect(), backward.rect());
    }

    /// The whole point of the threshold: a click with a shaky hand must not replace a good
    /// selection with a two-pixel one.
    #[test]
    fn a_click_that_barely_moves_leaves_the_selection_alone() {
        let mut selection = Selection::preset(desktop(), Rect::new(10, 10, 50, 50));
        // Press bare desktop, twitch by less than the threshold, release.
        selection.press(Point::new(150, 90));
        selection.motion(Point::new(151, 91));
        selection.release();
        assert_eq!(selection.rect(), Some(Rect::new(10, 10, 50, 50)));
    }

    #[test]
    fn a_band_past_the_threshold_replaces_the_selection() {
        let mut selection = Selection::preset(desktop(), Rect::new(10, 10, 50, 50));
        selection.press(Point::new(100, 10));
        selection.motion(Point::new(140, 40));
        selection.release();
        assert_eq!(selection.rect(), Some(Rect::new(100, 10, 40, 30)));
    }

    #[test]
    fn a_band_is_clamped_to_the_desktop() {
        let selection = dragged(Point::new(-50, -50), Point::new(500, 500));
        assert_eq!(selection.rect(), Some(desktop()));
    }

    #[test]
    fn the_grip_says_what_a_press_would_do() {
        let selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        assert_eq!(
            selection.grip_at(Point::new(40, 40)),
            Grip::Handle(Handle::TopLeft)
        );
        assert_eq!(selection.grip_at(Point::new(60, 60)), Grip::Inside);
        assert_eq!(selection.grip_at(Point::new(150, 90)), Grip::Outside);
        // Nothing selected: everything is bare desktop.
        assert_eq!(
            Selection::new(desktop()).grip_at(Point::new(60, 60)),
            Grip::Outside
        );
    }

    /// A handle sits on the last pixel inside the rectangle, not one past it.
    #[test]
    fn the_handles_sit_on_the_selection() {
        let rect = Rect::new(10, 20, 40, 30);
        assert_eq!(Handle::TopLeft.centre(rect), Point::new(10, 20));
        assert_eq!(Handle::BottomRight.centre(rect), Point::new(49, 49));
        assert_eq!(Handle::Right.centre(rect), Point::new(49, 35));
    }

    #[test]
    fn a_handle_resizes_from_the_opposite_edge() {
        let mut selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        selection.press(Point::new(79, 79)); // bottom-right
        selection.motion(Point::new(99, 89));
        selection.release();
        // The top-left corner has not moved.
        assert_eq!(selection.rect(), Some(Rect::new(40, 40, 60, 50)));
    }

    #[test]
    fn an_edge_handle_moves_only_its_own_edge() {
        let mut selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        selection.press(Point::new(40, 60)); // left edge
        selection.motion(Point::new(20, 95)); // pulled left and, pointlessly, down
        selection.release();
        assert_eq!(selection.rect(), Some(Rect::new(20, 40, 60, 40)));
    }

    /// Dragging a handle past the opposite edge flips the rectangle rather than collapsing it,
    /// which is what every other selection in the world does.
    #[test]
    fn a_handle_dragged_past_the_far_edge_flips() {
        let mut selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        selection.press(Point::new(79, 79));
        selection.motion(Point::new(10, 10));
        selection.release();
        // Pixels 10 through 40 inclusive on both axes: the anchor corner stays selected, which
        // is what makes the flip continuous rather than a jump.
        assert_eq!(selection.rect(), Some(Rect::new(10, 10, 31, 31)));
    }

    #[test]
    fn dragging_inside_moves_the_whole_selection() {
        let mut selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        // The middle, not near a corner -- (50, 50) is inside the top-left handle's grab area
        // on a selection this small, and would resize.
        selection.press(Point::new(60, 60));
        selection.motion(Point::new(80, 70));
        selection.release();
        assert_eq!(selection.rect(), Some(Rect::new(60, 50, 40, 40)));
    }

    /// A move must not also resize. Clipping against the edge would shrink the selection, and
    /// once shrunk there is no way to get the size back except by starting over.
    #[test]
    fn a_move_against_the_edge_slides_rather_than_shrinking() {
        let mut selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        selection.press(Point::new(60, 60));
        selection.motion(Point::new(500, 500));
        selection.release();
        let rect = selection.rect().unwrap();
        assert_eq!((rect.w, rect.h), (40, 40));
        assert_eq!(rect, Rect::new(160, 60, 40, 40));
    }

    #[test]
    fn arrows_nudge_and_shift_nudges_further() {
        let mut selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        assert!(selection.nudge(1, 0, false));
        assert_eq!(selection.rect(), Some(Rect::new(41, 40, 40, 40)));
        assert!(selection.nudge(NUDGE_FAST, 0, false));
        assert_eq!(selection.rect(), Some(Rect::new(51, 40, 40, 40)));
    }

    #[test]
    fn a_nudged_resize_grows_from_the_bottom_right() {
        let mut selection = Selection::preset(desktop(), Rect::new(40, 40, 40, 40));
        assert!(selection.nudge(5, -5, true));
        assert_eq!(selection.rect(), Some(Rect::new(40, 40, 45, 35)));
    }

    #[test]
    fn nudging_nothing_does_nothing() {
        let mut selection = Selection::new(desktop());
        assert!(!selection.nudge(1, 0, false));
        assert_eq!(selection.rect(), None);
    }

    /// `--select` comes from the compositor describing a window, which can hang off the edge
    /// of its monitor. A rectangle outside the desktop would crop to black.
    #[test]
    fn a_preset_is_clamped_to_the_desktop() {
        let selection = Selection::preset(desktop(), Rect::new(-20, -20, 400, 400));
        assert_eq!(selection.rect(), Some(desktop()));

        // Entirely off the desktop selects nothing rather than an empty rectangle.
        let gone = Selection::preset(desktop(), Rect::new(1000, 1000, 40, 40));
        assert_eq!(gone.rect(), None);
    }

    #[test]
    fn select_all_takes_the_whole_desktop() {
        let mut selection = Selection::new(desktop());
        assert!(selection.select_all());
        assert_eq!(selection.rect(), Some(desktop()));
        // Already everything: nothing changed, so nothing to redraw.
        assert!(!selection.select_all());
    }

    /// A corner and an edge handle overlap on a thin selection. The corner has to win, or one
    /// of the two axes cannot be resized at all.
    #[test]
    fn a_corner_beats_an_edge_where_they_overlap() {
        let selection = Selection::preset(desktop(), Rect::new(40, 40, 6, 6));
        assert!(matches!(
            selection.grip_at(Point::new(43, 40)),
            Grip::Handle(Handle::TopLeft | Handle::TopRight)
        ));
    }
}
