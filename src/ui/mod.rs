// SPDX-License-Identifier: GPL-3.0-or-later
//! The overlay, as a Wayland client.
//!
//! One **wlr-layer-shell overlay surface per monitor**, each showing the part of the frozen
//! desktop that falls on its own screen. Overlay rather than top, because the point is to be
//! above everything including a fullscreen window; `KeyboardInteractivity::Exclusive`, because
//! Escape has to work before the user has clicked anything.
//!
//! ## One picture, several panes
//!
//! The selection lives in **global desktop coordinates** and never learns there is more than
//! one monitor. Each pane knows only where its own top-left corner sits in that plane, adds it
//! to every pointer event and subtracts it from everything it draws. That is what lets a band
//! be dragged across the gap between two screens, which is the whole reason the model was
//! written that way -- see [`crate::select`] and [`crate::shot`].
//!
//! ## Order of operations
//!
//! The screen is frozen *before* any surface is mapped. It is not a detail: the overlay covers
//! the desktop, so capturing afterwards would photograph the dialog asking about the picture.
//!
//! ## Scale
//!
//! Everything is drawn at scale 1, as `wlrix-desktop` does, and a HiDPI monitor is reported
//! rather than handled -- see [`App::warn_if_scaled`]. A screenshot is exactly where fractional
//! scale would show, so this is worth being loud about rather than quietly wrong.

pub mod paint;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::calloop::{EventLoop, LoopHandle},
    reexports::calloop_wayland_source::WaylandSource,
    reexports::client::{
        Connection, EventQueue, QueueHandle,
        globals::registry_queue_init,
        protocol::{
            wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_seat::WlSeat, wl_shm,
            wl_surface::WlSurface,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{
            CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wlrix_ui::canvas::{Canvas, Rect};
use wlrix_ui::palette::Palette;
use wlrix_ui::text::Fonts;

use crate::config::Config;
use crate::select::{Grip, Point, Selection};
use crate::shot::Shot;
use crate::wayland::Wayland;
use paint::{Bar, Button};

/// `wl_pointer`'s left button, from `linux/input-event-codes.h`.
const BTN_LEFT: u32 = 0x110;

/// What the user settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Save(Rect),
    Copy(Rect),
    Canceled,
}

/// A connected client, before it has anything on screen.
///
/// Handed back from [`connect`] so the caller can freeze the screen and *then* decide whether
/// to put an overlay up at all -- `--no-ui` never does.
pub struct Client {
    pub app: App,
    pub queue: EventQueue<App>,
    pub connection: Connection,
    event_loop: EventLoop<'static, App>,
}

/// One monitor's overlay surface.
struct Pane {
    layer: LayerSurface,
    /// Where this pane's top-left corner sits on the desktop.
    origin: (i32, i32),
    width: i32,
    height: i32,
    configured: bool,
}

/// The whole overlay's state, and the dispatch target for every protocol in the program.
pub struct App {
    registry_state: RegistryState,
    pub output_state: OutputState,
    seat_state: SeatState,
    pub shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    pool: Option<SlotPool>,
    pub qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,

    /// The globals `smithay-client-toolkit` does not wrap. See [`crate::wayland`].
    pub wayland: Wayland,

    panes: Vec<Pane>,
    /// The frozen desktop. Empty until [`crate::grab`] has run.
    shot: Shot,
    selection: Selection,
    bar: Option<Bar>,

    pointer: Option<ThemedPointer>,
    keyboard: Option<WlKeyboard>,
    modifiers: Modifiers,
    /// Where the pointer is, in global desktop coordinates.
    at: Point,
    cursor: Option<CursorIcon>,
    hovered: Option<Button>,
    pressed: Option<Button>,

    palette: &'static Palette,
    fonts: Fonts,
    dim: u8,

    choice: Option<Choice>,
    dirty: bool,
}

/// Connect, bind everything, and settle the output layout.
///
/// Returns before anything is on screen. Two roundtrips, and both are needed: the globals are
/// bound during the first, and an output's `xdg_output` logical position -- which is how panes
/// are placed relative to one another -- only arrives during the second.
pub fn connect(config: &Config) -> Result<Client, String> {
    let fonts = Fonts::load().map_err(|err| format!("no usable font: {err}"))?;
    let (palette, unknown) = wlrix_ui::palette::resolve(config.appearance.palette.as_deref());
    if let Some(why) = unknown {
        eprintln!("wlrix-screenshot: {why}; using {}", palette.id);
    }

    let connection = Connection::connect_to_env()
        .map_err(|err| format!("no Wayland compositor to connect to: {err}"))?;
    let (globals, mut queue) = registry_queue_init(&connection)
        .map_err(|err| format!("could not read the registry: {err}"))?;
    let qh = queue.handle();

    let event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|err| format!("could not create the event loop: {err}"))?;
    let loop_handle = event_loop.handle();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(|err| format!("wl_shm unavailable: {err}"))?,
        compositor: CompositorState::bind(&globals, &qh)
            .map_err(|err| format!("wl_compositor unavailable: {err}"))?,
        layer_shell: LayerShell::bind(&globals, &qh)
            .map_err(|err| format!("wlr-layer-shell unavailable: {err}"))?,
        // Sized once the monitors are known: a screen's worth of pixels per pane is not a
        // guess worth making before there is anything to guess from.
        pool: None,
        qh: qh.clone(),
        loop_handle,
        wayland: Wayland::default(),
        panes: Vec::new(),
        shot: Shot {
            regions: Vec::new(),
        },
        selection: Selection::new(Rect::new(0, 0, 0, 0)),
        bar: None,
        pointer: None,
        keyboard: None,
        modifiers: Modifiers::default(),
        at: Point::new(0, 0),
        cursor: None,
        hovered: None,
        pressed: None,
        palette,
        fonts,
        dim: config.appearance.dim_amount(),
        choice: None,
        dirty: true,
    };

    app.wayland.output_sources = app.registry_state.bind_one(&qh, 1..=1, ()).ok();
    app.wayland.copy = app.registry_state.bind_one(&qh, 1..=1, ()).ok();
    // Optional: without it the Copy button is not offered. See `crate::clipboard`.
    app.wayland.data_control = app.registry_state.bind_one(&qh, 1..=2, ()).ok();

    for stage in ["bind the globals", "read the output layout"] {
        queue
            .roundtrip(&mut app)
            .map_err(|err| format!("could not {stage}: {err}"))?;
    }

    Ok(Client {
        app,
        queue,
        connection,
        event_loop,
    })
}

/// Put the overlay up and wait for an answer.
///
/// Consumes the client: the overlay is the last thing this process does before it saves,
/// copies, or gives up.
pub fn run(client: Client, shot: Shot, preset: Option<Rect>) -> Result<Choice, String> {
    let Client {
        mut app,
        queue,
        connection,
        mut event_loop,
    } = client;

    let bounds = shot.bounds();
    app.selection = match preset {
        Some(rect) => Selection::preset(bounds, rect),
        None => Selection::new(bounds),
    };
    app.shot = shot;
    app.refresh_bar();
    app.open_panes()?;

    WaylandSource::new(connection, queue)
        .insert(app.loop_handle.clone())
        .map_err(|err| format!("could not drive Wayland from the loop: {err}"))?;

    while app.choice.is_none() {
        event_loop
            .dispatch(None, &mut app)
            .map_err(|err| format!("event loop failed: {err}"))?;
        app.draw();
    }
    Ok(app.choice.unwrap_or(Choice::Canceled))
}

impl App {
    /// A monitor's connector name, which is how [`crate::grab`] labels a capture.
    pub fn output_name(&self, output: &WlOutput) -> Option<String> {
        let info = self.output_state.info(output)?;
        info.name.clone()
    }

    /// Where a monitor's top-left corner sits on the desktop.
    ///
    /// `logical_position` first, which is `xdg_output`'s answer and the one that accounts for
    /// scaling; `location` is `wl_output.geometry`'s and is what is left when the compositor
    /// has no `xdg_output`.
    pub fn output_origin(&self, name: &str) -> Option<(i32, i32)> {
        let info = self
            .output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .find(|info| info.name.as_deref() == Some(name))?;
        Some(info.logical_position.unwrap_or(info.location))
    }

    /// Say so, once, if a monitor's captured size is not its logical size.
    ///
    /// That is a scaled monitor, and this program draws everything at scale 1 -- the selection
    /// would be a fraction of the size the user aimed at. Reported rather than corrected: a
    /// HiDPI pass is real work, and silently handing back the wrong rectangle would be worse
    /// than saying which monitor cannot be trusted.
    pub fn warn_if_scaled(&self, name: &str, width: i32, height: i32) {
        let Some(info) = self
            .output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .find(|info| info.name.as_deref() == Some(name))
        else {
            return;
        };
        if let Some((lw, lh)) = info.logical_size
            && (lw, lh) != (width, height)
        {
            eprintln!(
                "wlrix-screenshot: {name} is {width}x{height} pixels but {lw}x{lh} logical \
                 (scale {}); this program draws at scale 1, so the overlay on that monitor \
                 will not line up",
                info.scale_factor,
            );
        }
    }

    /// Create one overlay surface per monitor that was captured.
    ///
    /// Only the monitors in the shot: a monitor that was asleep during the grab has no pixels
    /// to show, and covering it with a black overlay would be worse than leaving it alone.
    fn open_panes(&mut self) -> Result<(), String> {
        let mut total = 0usize;
        let regions: Vec<Rect> = self.shot.regions.iter().map(|region| region.rect).collect();

        for output in self.output_state.outputs() {
            let Some(info) = self.output_state.info(&output) else {
                continue;
            };
            let origin = info.logical_position.unwrap_or(info.location);
            let Some(rect) = regions
                .iter()
                .find(|rect| (rect.x, rect.y) == origin)
                .copied()
            else {
                continue;
            };

            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(
                &self.qh,
                surface,
                Layer::Overlay,
                Some("wlrix-screenshot"),
                Some(&output),
            );
            layer.set_anchor(Anchor::all());
            // `-1`, not `0`. Zero means "shrink me so I do not cover anyone's exclusive zone",
            // which is right for a desktop and wrong here: a panel's strip of screen is part of
            // the screenshot, and an overlay that stopped short of it would leave a live strip
            // of desktop showing through the frozen one.
            layer.set_exclusive_zone(-1);
            // Exclusive, so Escape works before anything has been clicked. The compositor has
            // to honor this on map rather than on first click -- until it does, keys go to
            // whatever window is underneath, invisibly.
            layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            layer.set_size(rect.w as u32, rect.h as u32);
            layer.commit();

            total += (rect.w * rect.h) as usize * 4;
            self.panes.push(Pane {
                layer,
                origin,
                width: rect.w,
                height: rect.h,
                configured: false,
            });
        }

        if self.panes.is_empty() {
            return Err("no monitor to put the overlay on".to_string());
        }
        self.pool = Some(
            SlotPool::new(total.max(1), &self.shm)
                .map_err(|err| format!("could not create a buffer pool: {err}"))?,
        );
        Ok(())
    }

    /// Recompute where the button bar goes. Called whenever the selection moves.
    fn refresh_bar(&mut self) {
        // Hidden while a drag is in progress: the bar follows the selection, and one sliding
        // around under the pointer is both distracting and, at the moment of release, in the
        // way of the thing being released onto.
        let Some(rect) = self.selection.rect().filter(|_| !self.selection.dragging()) else {
            self.bar = None;
            return;
        };
        // Copy is offered only where the compositor has `wlr-data-control`. A button that
        // cannot work is worse than one that is not there.
        let can_copy = self.wayland.data_control.is_some();
        self.bar = Some(paint::layout(
            rect,
            self.selection.bounds(),
            &mut self.fonts,
            |button| button != Button::Copy || can_copy,
        ));
    }

    /// Redraw every configured pane, if anything has changed.
    fn draw(&mut self) {
        if !self.dirty || self.choice.is_some() {
            return;
        }
        self.dirty = false;

        for index in 0..self.panes.len() {
            if !self.panes[index].configured {
                continue;
            }
            if let Err(err) = self.draw_pane(index) {
                eprintln!("wlrix-screenshot: could not draw the overlay: {err}");
            }
        }
    }

    fn draw_pane(&mut self, index: usize) -> Result<(), String> {
        let (origin, width, height) = {
            let pane = &self.panes[index];
            (pane.origin, pane.width, pane.height)
        };
        let Some(pool) = &mut self.pool else {
            return Err("no buffer pool".to_string());
        };
        let (buffer, pixels) = pool
            .create_buffer(
                width,
                height,
                width * 4,
                // Opaque: the overlay covers the screen completely and there is nothing behind
                // it worth compositing against, so the compositor can skip whatever is under.
                wl_shm::Format::Xrgb8888,
            )
            .map_err(|err| format!("could not get a buffer: {err}"))?;

        let mut canvas = Canvas::new(pixels, width, height);
        let scene = paint::Scene {
            origin,
            shot: &self.shot,
            selection: &self.selection,
            bar: self.bar.as_ref(),
            hovered: self.hovered,
            pressed: self.pressed,
            palette: self.palette,
            dim: self.dim,
        };
        paint::paint(&mut canvas, &mut self.fonts, &scene);

        let pane = &self.panes[index];
        let surface = pane.layer.wl_surface();
        surface.damage_buffer(0, 0, width, height);
        buffer
            .attach_to(surface)
            .map_err(|err| format!("could not attach the buffer: {err}"))?;
        surface.commit();
        Ok(())
    }

    /// Which pane a surface belongs to.
    fn pane_of(&self, surface: &WlSurface) -> Option<usize> {
        self.panes
            .iter()
            .position(|pane| pane.layer.wl_surface() == surface)
    }

    /// Set the pointer to whatever is under it, when that has changed.
    ///
    /// Guarded on the shape actually changing: `set_cursor` is a protocol round of its own, and
    /// sending one per motion event would be a request per pixel of a drag.
    fn update_cursor(&mut self, conn: &Connection) {
        let on_bar = self
            .bar
            .as_ref()
            .is_some_and(|bar| bar.contains(self.at.x, self.at.y));
        let wanted = cursor_for(self.selection.grip_at(self.at), on_bar);
        if self.cursor == Some(wanted) {
            return;
        }
        self.cursor = Some(wanted);
        if let Some(pointer) = &self.pointer
            && let Err(err) = pointer.set_cursor(conn, wanted)
        {
            // Not fatal, and not worth repeating: the selection still works with whatever
            // cursor the compositor left in place.
            eprintln!("wlrix-screenshot: could not set the cursor: {err}");
        }
    }

    /// A button was released over the button it was pressed on.
    fn activate(&mut self, button: Button) {
        let Some(rect) = self.selection.rect() else {
            return;
        };
        self.choice = Some(match button {
            Button::Save => Choice::Save(rect),
            Button::Copy => Choice::Copy(rect),
            Button::Cancel => Choice::Canceled,
        });
    }

    /// One key press, however it arrived.
    ///
    /// Shared by `press_key` and by the repeat callback, which fires on a calloop timer and has
    /// no connection or serial to offer -- so neither is a parameter here. Nothing this does
    /// needs one.
    fn on_key(&mut self, event: smithay_client_toolkit::seat::keyboard::KeyEvent) {
        let step = if self.modifiers.shift {
            crate::select::NUDGE_FAST
        } else {
            1
        };
        // Ctrl turns the arrows from "move the selection" into "move its bottom-right corner",
        // which is the only way to size a selection to the pixel.
        let resize = self.modifiers.ctrl;

        let changed = match event.keysym {
            Keysym::Escape => {
                self.choice = Some(Choice::Canceled);
                return;
            }
            Keysym::Return | Keysym::KP_Enter => {
                if let Some(rect) = self.selection.rect() {
                    self.choice = Some(Choice::Save(rect));
                }
                return;
            }
            Keysym::c | Keysym::C if self.modifiers.ctrl => {
                if let Some(rect) = self.selection.rect() {
                    self.choice = Some(Choice::Copy(rect));
                }
                return;
            }
            Keysym::a | Keysym::A if self.modifiers.ctrl => self.selection.select_all(),
            Keysym::Left | Keysym::KP_Left => self.selection.nudge(-step, 0, resize),
            Keysym::Right | Keysym::KP_Right => self.selection.nudge(step, 0, resize),
            Keysym::Up | Keysym::KP_Up => self.selection.nudge(0, -step, resize),
            Keysym::Down | Keysym::KP_Down => self.selection.nudge(0, step, resize),
            _ => false,
        };
        if changed {
            self.refresh_bar();
        }
        self.touch(changed);
    }

    /// Note that something changed, so the next turn of the loop redraws.
    fn touch(&mut self, changed: bool) {
        self.dirty |= changed;
    }
}

/// The cursor shape for what is under the pointer.
///
/// Named shapes through `cursor-shape-v1`, which the compositor advertises, so these come out
/// as whatever the user's XCursor theme draws for them rather than as something this program
/// invented.
fn cursor_for(grip: Grip, on_bar: bool) -> CursorIcon {
    use crate::select::Handle;
    if on_bar {
        return CursorIcon::Default;
    }
    match grip {
        Grip::Outside => CursorIcon::Crosshair,
        Grip::Inside => CursorIcon::Move,
        Grip::Handle(handle) => match handle {
            Handle::TopLeft => CursorIcon::NwResize,
            Handle::Top => CursorIcon::NResize,
            Handle::TopRight => CursorIcon::NeResize,
            Handle::Left => CursorIcon::WResize,
            Handle::Right => CursorIcon::EResize,
            Handle::BottomLeft => CursorIcon::SwResize,
            Handle::Bottom => CursorIcon::SResize,
            Handle::BottomRight => CursorIcon::SeResize,
        },
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        // The compositor took the surface away -- a monitor unplugged, or the session locking.
        // There is no honest way to finish a selection whose screen has gone, so this ends the
        // same way Escape does rather than saving a rectangle of somewhere that no longer is.
        self.choice = Some(Choice::Canceled);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(index) = self.pane_of(layer.wl_surface()) else {
            return;
        };
        let (width, height) = configure.new_size;
        // A zero means "you choose", and what this chose is already in `width`/`height`.
        if width != 0 && height != 0 {
            self.panes[index].width = width as i32;
            self.panes[index].height = height as i32;
        }
        self.panes[index].configured = true;
        self.dirty = true;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
        // Everything is drawn at scale 1; see the module docs and `warn_if_scaled`.
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: smithay_client_toolkit::reexports::client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
        // Nothing is animated. Redraws are driven by input, not by the clock.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(index) = self.pane_of(&event.surface) else {
                continue;
            };
            let origin = self.panes[index].origin;
            // Surface-local to global, which is the only place the two coordinate systems meet.
            self.at = Point::new(
                origin.0 + event.position.0 as i32,
                origin.1 + event.position.1 as i32,
            );

            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let on_button = self
                        .bar
                        .as_ref()
                        .and_then(|bar| bar.button_at(self.at.x, self.at.y));
                    let hover_changed = self.hovered != on_button;
                    self.hovered = on_button;

                    // A press that landed on a button owns the pointer until it is released:
                    // sliding off and back must re-arm the button rather than start a band.
                    let moved = if self.pressed.is_some() {
                        false
                    } else {
                        self.selection.motion(self.at)
                    };
                    if moved {
                        self.refresh_bar();
                    }
                    self.touch(moved || hover_changed);
                    self.update_cursor(conn);
                }
                PointerEventKind::Leave { .. } => {
                    let changed = self.hovered.is_some();
                    self.hovered = None;
                    self.touch(changed);
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    if let Some(pressed) = self
                        .bar
                        .as_ref()
                        .and_then(|bar| bar.button_at(self.at.x, self.at.y))
                    {
                        self.pressed = Some(pressed);
                        self.touch(true);
                        continue;
                    }
                    // Anywhere else on the bar swallows the press. Without this, clicking the
                    // gap between two buttons would fall through and start a new band under
                    // the bar the user was aiming at.
                    if self
                        .bar
                        .as_ref()
                        .is_some_and(|bar| bar.contains(self.at.x, self.at.y))
                    {
                        continue;
                    }
                    let changed = self.selection.press(self.at);
                    // The bar goes away for the duration of the drag.
                    self.refresh_bar();
                    self.touch(true);
                    self.touch(changed);
                }
                PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                    if let Some(pressed) = self.pressed.take() {
                        self.touch(true);
                        if self.hovered == Some(pressed) {
                            self.activate(pressed);
                        }
                        continue;
                    }
                    let changed = self.selection.release();
                    self.refresh_bar();
                    self.touch(true);
                    self.touch(changed);
                    self.update_cursor(conn);
                }
                _ => {}
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.on_key(event);
    }
    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            // A themed pointer, so the crosshair and the eight resize shapes come from the
            // user's XCursor theme -- `ThemeSpec::System` reads the `XCURSOR_THEME` and
            // `XCURSOR_SIZE` the compositor announces to the session.
            let surface = self.compositor.create_surface(qh);
            match self.seat_state.get_pointer_with_theme(
                qh,
                &seat,
                self.shm.wl_shm(),
                surface,
                ThemeSpec::System,
            ) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(err) => eprintln!("wlrix-screenshot: no pointer: {err}"),
            }
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            // With repeat, so holding an arrow nudges continuously. Sizing a selection to the
            // pixel one key press at a time is not something anyone would sit through.
            match self.seat_state.get_keyboard_with_repeat(
                qh,
                &seat,
                None,
                self.loop_handle.clone(),
                Box::new(|app: &mut App, _keyboard, event| app.on_key(event)),
            ) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                Err(err) => eprintln!("wlrix-screenshot: no keyboard: {err}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer => self.pointer = None,
            Capability::Keyboard => {
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_layer!(App);
delegate_registry!(App);
