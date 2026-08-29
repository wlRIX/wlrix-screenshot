// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX screenshots, as a library.
//!
//! The binary (`main.rs`) is a thin shell over this, so the selection model, the cropping and
//! the file naming can be exercised by tests without a compositor -- the same split
//! `wlrix-desktop` draws for the same reason.

pub mod clipboard;
pub mod config;
pub mod grab;
pub mod portal;
pub mod save;
pub mod select;
pub mod shot;
pub mod ui;
pub mod wayland;
pub mod xdg;

pub use ui::App;
