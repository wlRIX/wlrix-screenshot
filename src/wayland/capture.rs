// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading pixels out of the compositor: `ext-image-copy-capture-v1`.
//!
//! Ported from `xdg-desktop-portal-wlrix/src/wayland/capture.rs` with the dmabuf negotiation
//! dropped -- see [`super::shm`] for why a single frame does not want it. The rest is the same
//! protocol driven the same way, and the two comments below are the two things about it that
//! are easy to get wrong.
//!
//! A *session* is opened against a source and lives as long as the thing being captured does.
//! The compositor announces what size and format of buffer it wants, and re-announces whenever
//! that changes. Against that session, a *frame* is requested one at a time: attach a buffer,
//! ask, and wait for `ready`.
//!
//! **A screenshot asks for exactly one frame and then stops**, which is the one real difference
//! from the portal's use. It also means the wait is not optional:
//!
//! > The compositor fills a pending capture frame when its backend *next draws*. On a still
//! > screen that can be a while, so the grab has to wait on `ready` rather than assume a
//! > roundtrip produced anything. A test on a motionless desktop proves nothing.

use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_shm::Format};
use wayland_protocols::ext::{
    image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

use crate::App;

/// What the compositor says a buffer for this source must look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Constraints {
    pub width: u32,
    pub height: u32,
    pub format: Format,
}

/// How the requested frame ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The buffer holds a frame.
    Ready,
    /// This frame did not happen. The reason says whether retrying could help.
    Failed(String),
}

/// One capture session, against one output.
pub struct Capture {
    pub session: ExtImageCopyCaptureSessionV1,
    source: ExtImageCaptureSourceV1,
    /// Which output this reads, by name, so a finished frame can be routed back.
    pub output_name: String,
    /// Constraints accumulating between `done` events.
    incoming: Incoming,
    /// The last complete set. `None` until the first `done`.
    pub constraints: Option<Constraints>,
    /// The source is gone. Terminal.
    pub stopped: bool,
    /// The frame currently outstanding, if any.
    frame: Option<ExtImageCopyCaptureFrameV1>,
    /// How that frame ended, waiting to be collected.
    outcome: Option<Outcome>,
}

/// Constraint fields as they arrive, before the `done` that makes them a set.
#[derive(Default, Clone)]
struct Incoming {
    width: u32,
    height: u32,
    format: Option<Format>,
}

impl Capture {
    /// Open a session against a source.
    ///
    /// Takes ownership of `source`: the session is only meaningful while the source exists, so
    /// tying their lifetimes together removes a way to get it wrong.
    pub fn new(
        copy: &ExtImageCopyCaptureManagerV1,
        qh: &QueueHandle<App>,
        source: ExtImageCaptureSourceV1,
        output_name: String,
        with_cursor: bool,
    ) -> Self {
        let options = if with_cursor {
            ext_image_copy_capture_manager_v1::Options::PaintCursors
        } else {
            ext_image_copy_capture_manager_v1::Options::empty()
        };
        let session = copy.create_session(&source, options, qh, ());
        Self {
            session,
            source,
            output_name,
            incoming: Incoming::default(),
            constraints: None,
            stopped: false,
            frame: None,
            outcome: None,
        }
    }

    /// Whether a frame is outstanding.
    pub fn busy(&self) -> bool {
        self.frame.is_some()
    }

    /// Whether the frame this asked for has landed, one way or the other.
    ///
    /// Reads the outcome without taking it, so the grab can wait on every capture and then
    /// collect them in one pass.
    pub fn ready(&self) -> bool {
        self.outcome.is_some()
    }

    /// Ask for a frame into `buffer`.
    ///
    /// Does nothing if one is already outstanding or the session has stopped, so a caller
    /// driving this from the loop does not have to track either.
    pub fn request(
        &mut self,
        qh: &QueueHandle<App>,
        buffer: &wayland_client::protocol::wl_buffer::WlBuffer,
        width: i32,
        height: i32,
    ) {
        if self.busy() || self.stopped {
            return;
        }
        let frame = self.session.create_frame(qh, ());
        frame.attach_buffer(buffer);
        // The whole buffer: there is no previous frame to diff against, so there is no damage
        // to report that would be narrower than everything.
        frame.damage_buffer(0, 0, width, height);
        frame.capture();
        self.outcome = None;
        self.frame = Some(frame);
    }

    /// Collect a finished frame, if one finished.
    ///
    /// Destroys the frame object as it goes: a frame is single-use, and holding it would leak a
    /// protocol object per captured frame.
    pub fn take_outcome(&mut self) -> Option<Outcome> {
        let outcome = self.outcome.take()?;
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        Some(outcome)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.session.destroy();
        self.source.destroy();
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for App {
    fn event(
        app: &mut Self,
        session: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(capture) = app
            .wayland
            .captures
            .iter_mut()
            .find(|capture| &capture.session == session)
        else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                capture.incoming.width = width;
                capture.incoming.height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                // The first offered format wins: the compositor lists them in its own order of
                // preference, and both of the two it offers are 32-bit little-endian with the
                // same channel order, differing only in whether alpha means anything.
                if capture.incoming.format.is_none()
                    && let Ok(format) = format.into_result()
                {
                    capture.incoming.format = Some(format);
                }
            }
            // Everything since the last `done` is one consistent set.
            ext_image_copy_capture_session_v1::Event::Done => {
                let Some(format) = capture.incoming.format else {
                    eprintln!(
                        "wlrix-screenshot: the compositor offered no shm format for {}; \
                         it cannot be captured",
                        capture.output_name,
                    );
                    return;
                };
                capture.constraints = Some(Constraints {
                    width: capture.incoming.width,
                    height: capture.incoming.height,
                    format,
                });
                capture.incoming.format = None;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                capture.stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(capture) = app
            .wayland
            .captures
            .iter_mut()
            .find(|capture| capture.frame.as_ref() == Some(frame))
        else {
            return;
        };
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => capture.outcome = Some(Outcome::Ready),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                capture.outcome = Some(Outcome::Failed(format!("{reason:?}")));
            }
            // `Transform` is not acted on. wlRIX's compositor captures offscreen with
            // `Transform::Normal` on every path -- it had a bug where it passed the output's own
            // transform and `grim` came back upside down under winit, fixed by always using
            // Normal. Honoring the event here would reintroduce exactly that, and a screenshot
            // is where it would be most obvious.
            _ => {}
        }
    }
}
