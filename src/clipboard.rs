// SPDX-License-Identifier: GPL-3.0-or-later
//! Putting a screenshot on the clipboard.
//!
//! ## The clipboard owner has to outlive this program
//!
//! Wayland's clipboard is pull-based. Setting a selection does not hand the compositor any
//! data; it registers *this client* as the thing to ask, and the bytes are only sent when
//! somebody pastes. So a program that sets a selection and exits has set nothing -- the
//! selection dies with its owner, and the paste a second later finds an empty clipboard.
//!
//! Every clipboard tool solves this the same way, and so does this one: a second process is
//! started that does nothing but hold the selection and answer for it, and this one returns.
//! It gives up the selection when the compositor says somebody else has taken it.
//!
//! ## Why a child process rather than `fork`
//!
//! `wl-copy` forks. Forking out of *this* program would be forking a process that has already
//! loaded fonts and opened a compositor connection, and only async-signal-safe work is
//! defined in the child of a fork -- which font loading and Wayland are not. So instead the
//! program starts a fresh copy of itself through `/proc/self/exe` and hands the PNG over on
//! its stdin. The child inherits nothing but the bytes, and makes its own connection.
//!
//! `--serve-clipboard` is that mode. It is not in `--help`: it is an implementation detail of
//! this file, not something to run by hand.
//!
//! ## Why `wlr-data-control` rather than `wl_data_device`
//!
//! `wl_data_device.set_selection` needs a serial from a recent input event on a seat this
//! client has focus on. The clipboard owner is a background process with no window at all, so
//! it has neither. `zwlr_data_control_manager_v1` exists for exactly this and needs neither.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// The one type offered. Everything that takes an image from a clipboard takes this.
const MIME: &str = "image/png";
/// The hidden argument that turns a fresh copy of this program into the clipboard owner.
pub const SERVE_ARG: &str = "--serve-clipboard";

/// Hand the image to a clipboard owner and return.
///
/// Returns as soon as the bytes are written, without waiting for the child to claim the
/// selection: there is nothing useful to do with the answer, and the caller is on its way out.
pub fn copy(png: &[u8]) -> Result<(), String> {
    let mut child = Command::new("/proc/self/exe")
        .arg(SERVE_ARG)
        .stdin(Stdio::piped())
        // No stdout, deliberately. The portal watches this program's stdout for the answer and
        // takes its close as "finished"; a child holding a copy of it open would leave the
        // portal waiting on a pipe that nothing is ever going to write to again.
        .stdout(Stdio::null())
        .spawn()
        .map_err(|err| format!("could not start the clipboard owner: {err}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or("the clipboard owner has no stdin")?;
    // Safe to block on: the child reads its stdin to the end before it does anything else, so
    // it cannot be waiting on this program for anything while this program waits on the pipe.
    stdin
        .write_all(png)
        .map_err(|err| format!("could not send the image to the clipboard owner: {err}"))?;
    drop(stdin);
    Ok(())
}

/// Be the clipboard owner: read the image, claim the selection, and answer for it.
///
/// Returns when the selection is taken by somebody else, which is the compositor's way of
/// saying this data is no longer the clipboard.
pub fn serve() -> Result<(), String> {
    let mut png = Vec::new();
    std::io::stdin()
        .read_to_end(&mut png)
        .map_err(|err| format!("could not read the image: {err}"))?;
    if png.is_empty() {
        return Err("no image on stdin".to_string());
    }

    let connection = Connection::connect_to_env()
        .map_err(|err| format!("no Wayland compositor to connect to: {err}"))?;
    let (globals, mut queue) = registry_queue_init::<Owner>(&connection)
        .map_err(|err| format!("could not read the registry: {err}"))?;
    let qh = queue.handle();

    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|err| format!("the compositor has no wlr-data-control: {err}"))?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|err| format!("the compositor has no seat: {err}"))?;

    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    source.offer(MIME.to_string());
    device.set_selection(Some(&source));

    let mut owner = Owner {
        png,
        source: source.clone(),
        done: false,
    };
    // Round-trip first, so the selection is really claimed before this reports success -- and
    // so a protocol error (an unsupported mime, a compositor that refused) is seen here rather
    // than in a silence the user reads as "copy did nothing".
    queue
        .roundtrip(&mut owner)
        .map_err(|err| format!("could not claim the clipboard: {err}"))?;

    while !owner.done {
        queue
            .blocking_dispatch(&mut owner)
            .map_err(|err| format!("lost the compositor: {err}"))?;
    }
    Ok(())
}

/// The clipboard owner's whole state.
struct Owner {
    png: Vec<u8>,
    source: ZwlrDataControlSourceV1,
    done: bool,
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for Owner {
    fn event(
        owner: &mut Self,
        _source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if mime_type != MIME {
                    // Only one type was offered, so this is a compositor bug or a confused
                    // consumer. Closing the descriptor is the honest answer: the reader sees
                    // an empty paste rather than a PNG labeled as something else.
                    return;
                }
                if let Err(err) = write_all(fd, &owner.png) {
                    eprintln!("wlrix-screenshot: could not send the clipboard image: {err}");
                }
            }
            // Somebody else took the clipboard. This data is no longer reachable, so there is
            // nothing left to hold open.
            zwlr_data_control_source_v1::Event::Cancelled => {
                owner.source.destroy();
                owner.done = true;
            }
            _ => {}
        }
    }
}

/// Write the whole image to the descriptor the consumer handed over.
///
/// Taken by value: the descriptor arrives owned, and it has to be *closed* when the write is
/// done. A consumer reads until end of file, so a descriptor left open is a paste that never
/// finishes -- which looks like the application hanging, not like this program leaking one.
fn write_all(fd: OwnedFd, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)?;
    file.flush()
    // `file` is dropped here, which closes it. That close is the end-of-file the consumer is
    // waiting for.
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Owner {
    fn event(
        _owner: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        _event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The device reports what *others* have put on the clipboard, which this program has
        // no interest in. `Canceled` on the source above is the only event that matters.
    }

    // `data_offer` and `selection` carry new objects, so the queue has to be told what to make
    // of them even though nothing here reads from them.
    wayland_client::event_created_child!(Owner, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

/// Objects with no events worth acting on, and the registry, which after `registry_queue_init`
/// only reports globals coming and going.
macro_rules! ignore_events {
    ($($ty:ty $(: $data:ty)?),* $(,)?) => {$(
        impl Dispatch<$ty, ignore_events!(@data $($data)?)> for Owner {
            fn event(
                _owner: &mut Self,
                _obj: &$ty,
                _event: <$ty as wayland_client::Proxy>::Event,
                _data: &ignore_events!(@data $($data)?),
                _conn: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
    (@data $data:ty) => { $data };
    (@data) => { () };
}
ignore_events!(
    WlRegistry: GlobalListContents,
    WlSeat,
    ZwlrDataControlManagerV1,
    ZwlrDataControlOfferV1,
);
