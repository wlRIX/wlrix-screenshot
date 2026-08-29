// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared-memory buffers for the compositor to write captures into.
//!
//! Ported from `xdg-desktop-portal-wlrix/src/wayland/shm.rs`, which allocates the same buffers
//! for the same protocol. What is gone is everything about PipeWire: that backend wraps
//! *PipeWire's* memfds so a shared frame is never copied, which is why its version carries a
//! `Region` with an offset into a file holding several images. A screenshot allocates one image
//! and reads it once, so a buffer is simply a file of its own.
//!
//! There is no dmabuf path here either. A dmabuf saves the readback, which is the cost that
//! matters at 60fps and is worth nothing at all for a single frame -- against the cost of
//! opening a render node, negotiating modifiers, and having a second way for the capture to
//! fail.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::{
    fs::{MemfdFlags, ftruncate, memfd_create},
    mm::{MapFlags, ProtFlags, mmap, munmap},
};
use wayland_client::{
    QueueHandle,
    protocol::{wl_buffer::WlBuffer, wl_shm::WlShm, wl_shm_pool::WlShmPool},
};

use super::capture::Constraints;
use crate::App;

/// Bytes per pixel for every format this program deals in.
///
/// Both formats the compositor offers -- `Xrgb8888` and `Argb8888` -- are 32-bit, as is the
/// `Argb8888` canvas `wlrix-ui` draws into. Named so the arithmetic below reads as something
/// other than a stray 4.
pub const BYTES_PER_PIXEL: usize = 4;

/// An anonymous file, mapped, holding one captured image.
pub struct Memory {
    fd: OwnedFd,
    map: *mut u8,
    len: usize,
}

// SAFETY: the mapping is owned by this struct and only reached through `&self`/`&mut self`; the
// raw pointer is what makes the compiler doubt it, not anything shared across threads.
unsafe impl Send for Memory {}

impl Memory {
    pub fn new(len: usize) -> Result<Self, String> {
        let fd = memfd_create(c"wlrix-screenshot", MemfdFlags::CLOEXEC)
            .map_err(|err| format!("memfd_create: {err}"))?;
        ftruncate(&fd, len as u64).map_err(|err| format!("ftruncate to {len}: {err}"))?;

        // SAFETY: a fresh memfd of exactly `len` bytes, mapped shared so the compositor's writes
        // through its own mapping of the same file are visible here.
        let map = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
        }
        .map_err(|err| format!("mmap {len} bytes: {err}"))?;

        Ok(Self {
            fd,
            map: map.cast(),
            len,
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// What the compositor wrote.
    ///
    /// Only sound to read after a frame's `ready`: before that the compositor may be writing,
    /// and a torn image is the best case.
    pub fn pixels(&self) -> &[u8] {
        // SAFETY: `map` covers `len` readable bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.map, self.len) }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region mapped in `new`, once.
        let _ = unsafe { munmap(self.map.cast(), self.len) };
    }
}

/// A `wl_buffer` and the pool behind it, destroyed together.
///
/// Kept as a pair because destroying them in the wrong order, or forgetting the pool, leaks a
/// file descriptor per image.
pub struct Buffer {
    pub buffer: WlBuffer,
    pool: WlShmPool,
    pub width: i32,
    pub height: i32,
}

impl Buffer {
    /// Wrap a memfd as a buffer the compositor can write a capture into.
    pub fn new(
        shm: &WlShm,
        qh: &QueueHandle<App>,
        memory: &Memory,
        constraints: Constraints,
    ) -> Self {
        let (width, height) = (constraints.width as i32, constraints.height as i32);
        let stride = width * BYTES_PER_PIXEL as i32;
        let pool = shm.create_pool(memory.as_fd(), memory.len as i32, qh, ());
        let buffer = pool.create_buffer(0, width, height, stride, constraints.format, qh, ());
        Self {
            buffer,
            pool,
            width,
            height,
        }
    }

    /// How many bytes an image of this size needs.
    pub fn size_for(width: i32, height: i32) -> usize {
        width as usize * height as usize * BYTES_PER_PIXEL
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
