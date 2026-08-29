// SPDX-License-Identifier: GPL-3.0-or-later
//! The compositor side: freezing the screen, and putting the result on the clipboard.
//!
//! Three protocols, none of which `smithay-client-toolkit` wraps, so they are bound out of the
//! registry by hand and dispatched by the impls below and in [`capture`]:
//!
//! - **`ext-image-capture-source-v1`** — names *what* to capture. Only the output manager is
//!   bound. There is a per-toplevel one too, and it is deliberately unused: see [`grab`].
//! - **`ext-image-copy-capture-v1`** — negotiates buffers against a source and fills them.
//! - **`zwlr_data_control_manager_v1`** — the clipboard, without needing a seat serial or the
//!   keyboard focus. See [`crate::clipboard`].
//!
//! Everything else -- the compositor, the layer shell, the outputs, the seat, `wl_shm` -- comes
//! from `smithay-client-toolkit`, and its delegates and these hand-written impls sit on the one
//! [`App`] state side by side. That works because they never claim the same
//! `(interface, user data)` pair: SCTK's slot pool builds its pools and buffers through
//! `send_constructor` with an `ObjectData` of its own rather than through `Dispatch`.
//!
//! ## Why the toplevel capture source is not used
//!
//! It would be the obvious way to shoot a single window, and it is wrong here. The compositor
//! draws a window capture with `window.render_elements(...)` -- the client's own surface tree --
//! while wlRIX's 4Dwm frame is drawn *by the compositor*, outside that tree. A window shot that
//! way comes back with no titlebar. So every mode captures whole outputs, and "the active
//! window" is a rectangle the compositor hands over on the command line, having applied
//! `decoration::frame_rect` itself. See [`grab`].

pub mod capture;
pub mod shm;

use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm_pool::WlShmPool},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;

use crate::App;

/// The globals this program binds itself, and the capture sessions running against them.
#[derive(Default)]
pub struct Wayland {
    pub output_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub copy: Option<ExtImageCopyCaptureManagerV1>,
    /// The clipboard manager. Optional: a compositor without `wlr-data-control` still takes
    /// screenshots, it just cannot be asked to copy one -- which is a button to gray out
    /// rather than a reason to refuse to start.
    pub data_control: Option<ZwlrDataControlManagerV1>,
    /// One session per output, live only during the grab.
    pub captures: Vec<capture::Capture>,
}

impl Wayland {
    /// Make the capture source naming an output.
    pub fn output_source(
        &self,
        qh: &QueueHandle<App>,
        output: &WlOutput,
    ) -> Option<ExtImageCaptureSourceV1> {
        Some(self.output_sources.as_ref()?.create_source(output, qh, ()))
    }

    /// What is missing that would stop a screenshot happening at all.
    ///
    /// Checked once, before anything is on screen. A compositor that does not implement these
    /// cannot be made to later, and putting a full-screen overlay up and *then* discovering
    /// there is nothing to draw on it would leave the user looking at a black rectangle with
    /// no way to know why.
    pub fn missing_globals(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.output_sources.is_none() {
            missing.push("ext_output_image_capture_source_manager_v1");
        }
        if self.copy.is_none() {
            missing.push("ext_image_copy_capture_manager_v1");
        }
        missing
    }
}

/// Objects this program drives but reads no events from.
///
/// The managers are pure factories, and the pools and buffers here have no event worth acting
/// on -- a `wl_buffer.release` matters to a client that recycles buffers on its own schedule,
/// and this one is told a frame is done by the capture protocol instead.
macro_rules! ignore_events {
    ($($ty:ty),* $(,)?) => {$(
        impl Dispatch<$ty, ()> for App {
            fn event(
                _app: &mut Self,
                _obj: &$ty,
                _event: <$ty as Proxy>::Event,
                _data: &(),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}
ignore_events!(
    WlShmPool,
    WlBuffer,
    ExtImageCaptureSourceV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCopyCaptureManagerV1,
    ZwlrDataControlManagerV1,
);
