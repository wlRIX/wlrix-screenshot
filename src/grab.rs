// SPDX-License-Identifier: GPL-3.0-or-later
//! Freezing the screen: one capture per monitor, before anything is drawn over it.
//!
//! This runs on the raw event queue, before the overlay exists and before the event loop
//! starts. The order is not incidental -- the overlay shows the frozen desktop, so it has to be
//! frozen first, or the picture would contain the dialog asking about it.
//!
//! ## Every mode captures whole outputs
//!
//! Even "the active window". `ext-image-capture-source-v1` has a per-toplevel source and it is
//! deliberately not used: the compositor draws a window capture from the client's own surface
//! tree, and wlRIX's 4Dwm frame is drawn outside that tree, so a window shot that way loses its
//! titlebar. The active window arrives instead as a rectangle on the command line -- from the
//! compositor, which knows the focused window and owns `decoration::frame_rect`. One capture
//! path, and the frame comes out right by construction.
//!
//! ## Why there is a deadline
//!
//! The compositor fills a pending capture when its backend next draws, and asking for a frame
//! is itself a redraw request (`image_capture.rs`, `fn frame`), so a still desktop is not a
//! problem. A monitor that is *asleep* is: the render loop skips a powered-off output, so its
//! capture would never complete and this would wait forever with nothing on screen to say why.
//! After [`DEADLINE`] the shot is built from whatever arrived.

use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::client::{Connection, EventQueue};
use wlrix_ui::canvas::Rect;

use crate::App;
use crate::shot::{Region, Shot};
use crate::wayland::{capture::Capture, capture::Outcome, shm};

/// How long to wait for the compositor before giving up on an output.
///
/// Generously long for something that normally takes one repaint. This is not a frame budget;
/// it is the bound that keeps a sleeping monitor from hanging the program.
const DEADLINE: Duration = Duration::from_secs(2);

/// Capture every monitor.
///
/// Fails only when there is nothing at all to show. One output out of two failing is not an
/// error -- the shot covers the rest, with a line saying which one was lost, because a
/// screenshot of one screen beats no screenshot at all.
pub fn grab(
    app: &mut App,
    queue: &mut EventQueue<App>,
    connection: &Connection,
    cursor: bool,
) -> Result<Shot, String> {
    open_sessions(app, queue, cursor)?;
    // The constraints arrive asynchronously and there is nothing to allocate until they do.
    settle(app, queue, connection, |app| {
        app.wayland
            .captures
            .iter()
            .all(|capture| capture.constraints.is_some() || capture.stopped)
    })?;

    let buffers = request_frames(app, queue)?;
    settle(app, queue, connection, |app| {
        app.wayland
            .captures
            .iter()
            .all(|capture| capture.ready() || capture.stopped)
    })?;

    let shot = collect(app, buffers);
    // The sessions are finished with. Dropped here rather than left to the end of the program
    // so the compositor stops capturing the moment the overlay goes up -- otherwise it would
    // hold a session per monitor for as long as the user takes to choose.
    app.wayland.captures.clear();

    if shot.regions.is_empty() {
        return Err("nothing could be captured; is any monitor awake?".to_string());
    }
    Ok(shot)
}

/// Open a capture session against every output.
fn open_sessions(app: &mut App, queue: &mut EventQueue<App>, cursor: bool) -> Result<(), String> {
    let missing = app.wayland.missing_globals();
    if !missing.is_empty() {
        return Err(format!(
            "the compositor does not implement {} -- it cannot be captured",
            missing.join(", ")
        ));
    }
    let Some(copy) = app.wayland.copy.clone() else {
        return Err("no ext_image_copy_capture_manager_v1".to_string());
    };
    let qh = app.qh.clone();

    for output in app.output_state.outputs() {
        let name = app
            .output_name(&output)
            .unwrap_or_else(|| "an unnamed monitor".to_string());
        let Some(source) = app.wayland.output_source(&qh, &output) else {
            continue;
        };
        app.wayland
            .captures
            .push(Capture::new(&copy, &qh, source, name, cursor));
    }
    if app.wayland.captures.is_empty() {
        return Err("the compositor reported no monitors".to_string());
    }
    // So the requests above are on their way while the constraints come back.
    queue
        .flush()
        .map_err(|err| format!("could not talk to the compositor: {err}"))?;
    Ok(())
}

/// Allocate a buffer per capture and ask for its one frame.
///
/// The buffers are returned rather than stored on [`Capture`] because they outlive nothing:
/// the pixels are read out of them the moment the frames land, and holding them anywhere else
/// would keep a full-screen mapping alive for as long as the overlay is up.
fn request_frames(app: &mut App, queue: &mut EventQueue<App>) -> Result<Vec<Option<Held>>, String> {
    let qh = app.qh.clone();
    let wl_shm = app.shm.wl_shm().clone();
    let mut held = Vec::new();

    for index in 0..app.wayland.captures.len() {
        let Some(constraints) = app.wayland.captures[index].constraints else {
            held.push(None);
            continue;
        };
        let needed = shm::Buffer::size_for(constraints.width as i32, constraints.height as i32);
        let memory = match shm::Memory::new(needed) {
            Ok(memory) => memory,
            Err(err) => {
                eprintln!(
                    "wlrix-screenshot: no room for {}: {err}",
                    app.wayland.captures[index].output_name
                );
                held.push(None);
                continue;
            }
        };
        let buffer = shm::Buffer::new(&wl_shm, &qh, &memory, constraints);
        app.wayland.captures[index].request(&qh, &buffer.buffer, buffer.width, buffer.height);
        held.push(Some(Held { memory, buffer }));
    }

    queue
        .flush()
        .map_err(|err| format!("could not ask for a frame: {err}"))?;
    Ok(held)
}

/// A buffer and the memory behind it, alive only while its frame is in flight.
struct Held {
    memory: shm::Memory,
    buffer: shm::Buffer,
}

/// Turn the finished frames into the frozen desktop.
///
/// Two passes: every capture is collected first, with where its monitor sits in the logical
/// desktop and how big it is there, because the plane they all go into is scaled by the largest
/// of their scales -- see `shot`'s module comment -- and that is only known once all are in.
fn collect(app: &mut App, buffers: Vec<Option<Held>>) -> Shot {
    struct Taken {
        origin: (i32, i32),
        logical: (i32, i32),
        width: i32,
        height: i32,
        pixels: Vec<u8>,
    }
    let mut taken = Vec::new();
    for (index, held) in buffers.into_iter().enumerate() {
        let Some(capture) = app.wayland.captures.get_mut(index) else {
            continue;
        };
        let name = capture.output_name.clone();
        let (Some(held), Some(constraints)) = (held, capture.constraints) else {
            continue;
        };
        match capture.take_outcome() {
            Some(Outcome::Ready) => {}
            Some(Outcome::Failed(reason)) => {
                eprintln!("wlrix-screenshot: {name} could not be captured: {reason}");
                continue;
            }
            None => {
                eprintln!("wlrix-screenshot: {name} did not answer in time; leaving it out");
                continue;
            }
        }

        let Some((origin, logical)) = app.output_layout(&name) else {
            eprintln!("wlrix-screenshot: {name} has no place in the layout; leaving it out");
            continue;
        };
        let (width, height) = (constraints.width as i32, constraints.height as i32);

        // Copied out before `held` is dropped, which is what releases the mapping and destroys
        // the buffer. The frozen desktop has to own its pixels: the overlay redraws from them
        // on every pointer move, long after the compositor has stopped writing.
        taken.push(Taken {
            origin,
            // A compositor without `xdg_output` gives no logical size; the capture's own is the
            // best there is, and makes that monitor scale 1.
            logical: logical.unwrap_or((width, height)),
            width,
            height,
            pixels: held.memory.pixels()[..shm::Buffer::size_for(width, height)].to_vec(),
        });
        drop(held.buffer);
    }

    let scale = plane_scale(taken.iter().map(|t| (t.width, t.logical.0)));
    let regions = taken
        .into_iter()
        .map(|t| {
            let rect = plane_rect(t.origin, t.logical, (t.width, t.height), scale);
            Region::fit(rect, t.width, t.height, t.pixels)
        })
        .collect();
    Shot { regions, scale }
}

/// The plane's scale: the largest of the monitors' own, each its captured width over its
/// logical width. 1.0 with nothing scaled, or nothing captured.
fn plane_scale(monitors: impl Iterator<Item = (i32, i32)>) -> f64 {
    monitors
        .filter(|(_, logical)| *logical > 0)
        .map(|(pixels, logical)| pixels as f64 / logical as f64)
        .fold(1.0, f64::max)
}

/// Where a monitor goes in the plane. One at the plane's own scale keeps exactly the size it was
/// captured at -- rounding its logical size back up could disagree by a pixel and stretch the
/// whole picture to make it up -- and the others are scaled to meet it.
fn plane_rect(origin: (i32, i32), logical: (i32, i32), captured: (i32, i32), scale: f64) -> Rect {
    let x = (origin.0 as f64 * scale).round() as i32;
    let y = (origin.1 as f64 * scale).round() as i32;
    let own = if logical.0 > 0 {
        captured.0 as f64 / logical.0 as f64
    } else {
        1.0
    };
    if (own - scale).abs() < 1e-6 {
        return Rect::new(x, y, captured.0, captured.1);
    }
    Rect::new(
        x,
        y,
        (logical.0 as f64 * scale).round() as i32,
        (logical.1 as f64 * scale).round() as i32,
    )
}

/// Dispatch until `done`, or until the deadline passes.
///
/// Deliberately **not** `blocking_dispatch`, which is the obvious way to write this and cannot
/// be bounded: it waits for an event, so a compositor that sends nothing at all -- which is
/// exactly the sleeping-monitor case [`DEADLINE`] exists for -- would never return, and the
/// deadline below would never be looked at. So the read is prepared, the connection's fd is
/// polled with the time that is left, and only then are events taken.
fn settle(
    app: &mut App,
    queue: &mut EventQueue<App>,
    connection: &Connection,
    done: impl Fn(&App) -> bool,
) -> Result<(), String> {
    let until = Instant::now() + DEADLINE;
    loop {
        queue
            .dispatch_pending(app)
            .map_err(|err| format!("lost the compositor while capturing: {err}"))?;
        if done(app) {
            return Ok(());
        }
        let Some(remaining) = until.checked_duration_since(Instant::now()) else {
            // Not an error: `collect` leaves out whatever did not arrive, and says which.
            return Ok(());
        };

        // `None` means events arrived between the dispatch above and here, so there is
        // something to dispatch and nothing to wait for.
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        queue
            .flush()
            .map_err(|err| format!("could not talk to the compositor: {err}"))?;

        let mut fds = [rustix::event::PollFd::new(
            &connection,
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: remaining.as_secs() as _,
            tv_nsec: remaining.subsec_nanos() as _,
        };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => {
                drop(guard);
                return Ok(());
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {
                drop(guard);
                continue;
            }
            Err(err) => return Err(format!("could not wait on the compositor: {err}")),
        }
        guard
            .read()
            .map_err(|err| format!("lost the compositor while capturing: {err}"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plane_takes_the_largest_scale() {
        assert_eq!(plane_scale([(1920, 1920)].into_iter()), 1.0);
        assert_eq!(plane_scale([(2880, 1920), (1920, 1920)].into_iter()), 1.5);
        assert_eq!(plane_scale(std::iter::empty()), 1.0);
    }

    #[test]
    fn a_laptop_at_150_keeps_its_pixels_and_a_monitor_beside_it_is_scaled_up() {
        let laptop = plane_rect((0, 0), (1920, 1200), (2880, 1800), 1.5);
        assert_eq!(laptop, Rect::new(0, 0, 2880, 1800));
        // A 1920x1080 monitor at 100 %, to the right in the logical desktop.
        let external = plane_rect((1920, 0), (1920, 1080), (1920, 1080), 1.5);
        assert_eq!(external, Rect::new(2880, 0, 2880, 1620));
    }
}
