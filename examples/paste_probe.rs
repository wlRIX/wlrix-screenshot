// SPDX-License-Identifier: GPL-3.0-or-later
//! Read the clipboard, to check that `wlrix-screenshot --copy` really put something there.
//!
//! `wl-clipboard` is the usual way to check this and is not installed everywhere; this needs
//! nothing but the crate's own dependencies, and it speaks the same protocol the clipboard
//! owner does, so it exercises the exact path a paste takes. The same shape as the probes in
//! `wlrix-compositor/examples`.
//!
//! ```sh
//! cargo run --example paste_probe                 # list the offered types
//! cargo run --example paste_probe -- /tmp/x.png   # and save image/png to a file
//! ```

use std::io::Read;
use std::os::fd::OwnedFd;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};

const WANTED: &str = "image/png";

#[derive(Default)]
struct Probe {
    /// Types on the offer that is currently the selection.
    types: Vec<String>,
    /// The selection's offer, once the compositor has said which one it is.
    selection: Option<ZwlrDataControlOfferV1>,
    /// Set once `selection` has arrived, so the loop knows there is nothing more to wait for.
    settled: bool,
}

fn main() {
    let out = std::env::args().nth(1);

    let connection = Connection::connect_to_env().expect("no compositor");
    let (globals, mut queue) = registry_queue_init::<Probe>(&connection).expect("no registry");
    let qh = queue.handle();

    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .expect("the compositor has no wlr-data-control");
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).expect("no seat");
    let _device = manager.get_data_device(&seat, &qh, ());

    let mut probe = Probe::default();
    // Two rounds: the offer and its mime types arrive in the first, the `selection` event
    // naming which offer is the clipboard in the second.
    for _ in 0..2 {
        queue.roundtrip(&mut probe).expect("lost the compositor");
    }

    if !probe.settled || probe.selection.is_none() {
        println!("the clipboard is empty");
        std::process::exit(1);
    }
    println!("offered types: {}", probe.types.join(", "));

    let Some(path) = out else {
        return;
    };
    if !probe.types.iter().any(|mime| mime == WANTED) {
        println!("nothing on the clipboard is {WANTED}");
        std::process::exit(1);
    }

    // A pipe: the owner writes into one end and closes it, which is the end of the data.
    let (read, write) = rustix::pipe::pipe().expect("pipe");
    probe
        .selection
        .as_ref()
        .expect("checked above")
        .receive(WANTED.to_string(), write.as_fd());
    // The write end has to be closed *here* as well, or the read below never sees end of file:
    // this process would still be holding one open.
    drop(write);
    queue.flush().expect("flush");

    let mut bytes = Vec::new();
    std::fs::File::from(OwnedFd::from(read))
        .read_to_end(&mut bytes)
        .expect("read");
    std::fs::write(&path, &bytes).expect("write");
    println!("{} bytes of {WANTED} written to {path}", bytes.len());
}

use std::os::fd::AsFd;

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::Selection { id } => {
                probe.selection = id;
                probe.settled = true;
            }
            // A new offer starts a fresh list: the compositor sends the offer, then its types,
            // then says which offer is the selection.
            zwlr_data_control_device_v1::Event::DataOffer { .. } => probe.types.clear(),
            _ => {}
        }
    }

    wayland_client::event_created_child!(Probe, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            probe.types.push(mime_type);
        }
    }
}

macro_rules! ignore_events {
    ($($ty:ty $(: $data:ty)?),* $(,)?) => {$(
        impl Dispatch<$ty, ignore_events!(@data $($data)?)> for Probe {
            fn event(
                _probe: &mut Self,
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
    ZwlrDataControlSourceV1,
);
