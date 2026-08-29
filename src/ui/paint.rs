// SPDX-License-Identifier: GPL-3.0-or-later
//! Drawing the overlay: the frozen desktop, the selection over it, and the button bar.
//!
//! Layout lives here too, unlike `wlrix-ui`'s widgets, which draw and nothing else. The reason
//! is that the bar has to be *hit-tested* as well as drawn, and computing its rectangles in one
//! place and using them for both is what stops the two drifting -- a button that lights up
//! somewhere other than where it can be pressed is the classic way that goes wrong.
//!
//! Everything here works in **global** desktop coordinates and subtracts the pane's origin at
//! the last moment. That is what lets a selection, and the bar under it, span two monitors: each
//! pane draws the part of the same global picture that falls on its own screen.

use wlrix_ui::bevel::Bevel;
use wlrix_ui::canvas::{Canvas, Rect};
use wlrix_ui::color::Rgb;
use wlrix_ui::palette::Palette;
use wlrix_ui::text::{Face, Fonts, Run};
use wlrix_ui::{motif, widget};

use crate::select::{Handle, Selection, grab_radius};

/// How tall the button bar is.
const BAR_H: i32 = 32;
/// Padding inside the bar, and between the things in it.
const BAR_PAD: i32 = 6;
/// How far the bar sits from the selection.
const BAR_GAP: i32 = 8;
/// A button's height, and the padding either side of its label.
const BUTTON_H: i32 = BAR_H - BAR_PAD * 2;
const BUTTON_PAD: i32 = 14;
/// The smallest a button gets, so `OK`-length labels are not postage stamps.
const BUTTON_MIN_W: i32 = 64;
/// How big a handle is drawn. Smaller than [`crate::select::HANDLE_GRAB`] on purpose: a target
/// the size of its own artwork is a target most people miss.
const HANDLE_DRAW: i32 = 4;
/// Text size in the bar's size readout.
const READOUT_PX: f32 = 13.0;
/// Text size in the "click and drag" prompt.
const PROMPT_PX: f32 = 15.0;
/// Padding inside the prompt panel.
const PROMPT_PAD: i32 = 18;

/// What the user can press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Save,
    Copy,
    Cancel,
}

impl Button {
    /// Left to right, in the order the bar shows them.
    ///
    /// Cancel last and furthest from the pointer's likely resting place, which is where every
    /// dialog in the desktop puts the destructive answer.
    pub const ORDER: [Button; 3] = [Button::Save, Button::Copy, Button::Cancel];

    pub fn label(self) -> &'static str {
        match self {
            Button::Save => "Save",
            Button::Copy => "Copy",
            Button::Cancel => "Cancel",
        }
    }
}

/// Where the bar and its buttons are, in global coordinates.
pub struct Bar {
    pub rect: Rect,
    pub buttons: Vec<(Button, Rect)>,
    /// Where the `1280 × 800` readout goes.
    readout: Rect,
}

impl Bar {
    /// The button under a global point, if any.
    pub fn button_at(&self, x: i32, y: i32) -> Option<Button> {
        self.buttons
            .iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(button, _)| *button)
    }

    /// Whether a point is anywhere on the bar.
    ///
    /// Checked before the selection is: the bar floats *over* the frozen desktop, and a press
    /// on the gap between two buttons must not fall through and start a new band underneath it.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// Work out where the bar goes for this selection.
///
/// `enabled` decides which buttons are offered at all -- Copy is left out where the compositor
/// has no `wlr-data-control`, since a button that cannot work is worse than one that is not
/// there.
pub fn layout(
    selection: Rect,
    bounds: Rect,
    fonts: &mut Fonts,
    enabled: impl Fn(Button) -> bool,
) -> Bar {
    let mut widths = Vec::new();
    for button in Button::ORDER {
        if !enabled(button) {
            continue;
        }
        let text = fonts.width(Face::Regular, widget::BUTTON_PX, button.label());
        widths.push((button, (text + BUTTON_PAD * 2).max(BUTTON_MIN_W)));
    }

    // `1280 × 800` is the widest the readout gets on any screen anyone has; measured rather
    // than guessed so the bar does not change width as the selection is dragged.
    let readout_w = fonts.width(Face::Regular, READOUT_PX, "8888 \u{d7} 8888");
    let buttons_w: i32 = widths.iter().map(|(_, w)| w + BAR_PAD).sum();
    let bar_w = BAR_PAD * 2 + readout_w + BAR_PAD + buttons_w - BAR_PAD;

    // Under the selection by preference, above it when there is no room, and inside it when
    // there is no room either way -- a selection covering the whole screen has nowhere else
    // for the bar to be, and a bar off the edge is a bar that cannot be pressed.
    let below = selection.bottom() + BAR_GAP;
    let above = selection.top() - BAR_GAP - BAR_H;
    let y = if below + BAR_H <= bounds.bottom() {
        below
    } else if above >= bounds.top() {
        above
    } else {
        (selection.bottom() - BAR_GAP - BAR_H).max(bounds.top())
    };
    // Centred on the selection, then slid back onto the desktop.
    let x = (selection.left() + (selection.w - bar_w) / 2)
        .clamp(bounds.left(), (bounds.right() - bar_w).max(bounds.left()));
    let rect = Rect::new(x, y, bar_w, BAR_H);

    let readout = Rect::new(
        rect.left() + BAR_PAD,
        rect.top() + BAR_PAD,
        readout_w,
        BUTTON_H,
    );
    let mut cursor = readout.right() + BAR_PAD;
    let buttons = widths
        .into_iter()
        .map(|(button, w)| {
            let at = Rect::new(cursor, rect.top() + BAR_PAD, w, BUTTON_H);
            cursor += w + BAR_PAD;
            (button, at)
        })
        .collect();

    Bar {
        rect,
        buttons,
        readout,
    }
}

/// Everything one pane needs to draw itself.
pub struct Scene<'a> {
    /// Where this pane's top-left corner is on the desktop.
    pub origin: (i32, i32),
    pub shot: &'a crate::shot::Shot,
    pub selection: &'a Selection,
    pub bar: Option<&'a Bar>,
    /// Which button is under the pointer, and which is held down.
    pub hovered: Option<Button>,
    pub pressed: Option<Button>,
    pub palette: &'a Palette,
    /// How far the area outside the selection is darkened.
    pub dim: u8,
}

/// Paint one pane.
pub fn paint(canvas: &mut Canvas, fonts: &mut Fonts, scene: &Scene) {
    let (ox, oy) = scene.origin;
    let selection = scene.selection.rect();

    // The frozen desktop, dimmed everywhere the selection is not.
    //
    // Written as one pass over every pixel rather than a fill plus four dimmed rectangles
    // around it. It is the same work -- every pixel is touched either way -- and it cannot
    // get the seams wrong, which the four-rectangle version does the moment the selection
    // hangs off the edge of this pane.
    for y in 0..canvas.height() {
        for x in 0..canvas.width() {
            let (gx, gy) = (x + ox, y + oy);
            let pixel = Rgb(scene.shot.pixel(gx, gy));
            let inside = selection.is_some_and(|rect| rect.contains(gx, gy));
            let color = if inside {
                pixel
            } else {
                pixel.blend(scene.palette.locked, scene.dim)
            };
            canvas.put(x, y, color);
        }
    }

    match selection {
        Some(rect) => draw_selection(canvas, scene, rect),
        // Nothing chosen yet. The prompt is the only thing on screen that says the screen is
        // not simply frozen.
        None => draw_prompt(canvas, fonts, scene),
    }

    if let (Some(bar), Some(rect)) = (scene.bar, selection) {
        draw_bar(canvas, fonts, scene, bar, rect);
    }
}

/// The selection outline and its eight handles.
fn draw_selection(canvas: &mut Canvas, scene: &Scene, rect: Rect) {
    let (ox, oy) = scene.origin;
    let local = Rect::new(rect.x - ox, rect.y - oy, rect.w, rect.h);

    // Two outlines rather than one: a single line disappears against whatever it happens to
    // be drawn over, and a screenshot selection is over arbitrary pixels by definition.
    canvas.stroke_rect(local.inset(-1), scene.palette.outer_line);
    canvas.stroke_rect(local, scene.palette.select_fill);

    // Handles are squares, not circles: this is a Motif desktop, and a square handle with a
    // bevel is what a Motif sizing grip looks like.
    let radius = grab_radius(rect).min(HANDLE_DRAW);
    let bevel = Bevel::raised(
        scene.palette.face_top_shadow,
        scene.palette.face_bottom_shadow,
        1,
    );
    for handle in Handle::ALL {
        let centre = handle.centre(rect);
        let at = Rect::new(
            centre.x - ox - radius,
            centre.y - oy - radius,
            radius * 2 + 1,
            radius * 2 + 1,
        );
        motif::panel(canvas, at, scene.palette.face, bevel);
    }
}

/// The bar: a Motif panel, the size readout, and the buttons.
fn draw_bar(canvas: &mut Canvas, fonts: &mut Fonts, scene: &Scene, bar: &Bar, selection: Rect) {
    let (ox, oy) = scene.origin;
    let shift = |rect: Rect| Rect::new(rect.x - ox, rect.y - oy, rect.w, rect.h);

    motif::panel(
        canvas,
        shift(bar.rect),
        scene.palette.panel,
        Bevel::raised(
            scene.palette.panel_top_shadow,
            scene.palette.panel_bottom_shadow,
            2,
        ),
    );

    // The size, in the multiplication sign the rest of the desktop uses rather than an `x`.
    let readout = shift(bar.readout);
    let text = format!("{} \u{d7} {}", selection.w, selection.h);
    let baseline = readout.top() + (readout.h + fonts.ascent(Face::Regular, READOUT_PX)) / 2 - 2;
    fonts.draw(
        canvas,
        Run {
            face: Face::Regular,
            px: READOUT_PX,
            x: readout.left(),
            baseline,
            color: scene.palette.foreground,
        },
        &text,
    );

    for (button, rect) in &bar.buttons {
        // Pressed only while the pointer is still on the button it went down on, which is what
        // lets a press be taken back by sliding off before releasing.
        let held = scene.pressed == Some(*button) && scene.hovered == Some(*button);
        widget::button(
            canvas,
            fonts,
            scene.palette,
            shift(*rect),
            button.label(),
            held,
        );
    }
}

/// The panel shown before anything has been selected.
///
/// Centred on this pane rather than on the desktop, and drawn on every pane: with two monitors
/// there is no one place that is "the middle", and a prompt on the screen the user is not
/// looking at is a prompt they never read.
fn draw_prompt(canvas: &mut Canvas, fonts: &mut Fonts, scene: &Scene) {
    const TEXT: &str = "Click and drag to select an area.";
    let text_w = fonts.width(Face::Regular, PROMPT_PX, TEXT);
    let line_h = fonts.line_height(Face::Regular, PROMPT_PX);
    let panel = Rect::new(
        (canvas.width() - text_w) / 2 - PROMPT_PAD,
        (canvas.height() - line_h) / 2 - PROMPT_PAD,
        text_w + PROMPT_PAD * 2,
        line_h + PROMPT_PAD * 2,
    );
    motif::panel(
        canvas,
        panel,
        scene.palette.panel,
        Bevel::raised(
            scene.palette.panel_top_shadow,
            scene.palette.panel_bottom_shadow,
            2,
        ),
    );
    let baseline = fonts.centered_baseline(Face::Regular, PROMPT_PX, panel);
    fonts.draw(
        canvas,
        Run {
            face: Face::Regular,
            px: PROMPT_PX,
            x: panel.left() + PROMPT_PAD,
            baseline,
            color: scene.palette.foreground,
        },
        TEXT,
    );
}
