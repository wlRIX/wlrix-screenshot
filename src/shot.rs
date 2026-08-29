// SPDX-License-Identifier: GPL-3.0-or-later
//! The frozen desktop: what was captured, and how a rectangle of it becomes a PNG.
//!
//! One [`Region`] per monitor, each holding that monitor's pixels and where it sits in the
//! global layout. Everything above this treats the desktop as one image in global coordinates
//! and never learns there is more than one monitor -- which is what lets a selection be dragged
//! across the gap between two of them.
//!
//! ## Pixels
//!
//! The compositor writes `Xrgb8888` or `Argb8888`, both of which are a native-endian `u32` of
//! `0xAARRGGBB` -- so in memory, little-endian, the bytes are B, G, R, A. That is exactly the
//! layout `wlrix_ui::canvas::Canvas` draws into, so painting the frozen desktop into the
//! overlay is a straight copy rather than a conversion.
//!
//! ## Why the PNG has no alpha channel
//!
//! `Xrgb8888`'s fourth byte is **undefined**: the compositor is free to leave whatever was in
//! that memory. Writing it as an alpha channel produces a PNG that is transparent in patches,
//! which is the classic version of this bug and is invisible until somebody opens the file on a
//! white background. A screenshot of a desktop has no meaningful transparency anyway -- the
//! wallpaper is opaque -- so alpha is dropped rather than trusted, on both formats.

use wlrix_ui::canvas::Rect;

/// One monitor's worth of frozen desktop.
pub struct Region {
    /// Where this monitor sits in the global layout, and how big it is.
    pub rect: Rect,
    /// `rect.w * rect.h` pixels, four bytes each, in the layout described above.
    pub pixels: Vec<u8>,
}

impl Region {
    /// One pixel, or `None` off this monitor. Coordinates are **global**, not region-local.
    fn at(&self, x: i32, y: i32) -> Option<u32> {
        if !self.rect.contains(x, y) {
            return None;
        }
        let local_x = (x - self.rect.x) as usize;
        let local_y = (y - self.rect.y) as usize;
        let offset = (local_y * self.rect.w as usize + local_x) * 4;
        let bytes = self.pixels.get(offset..offset + 4)?;
        Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// The whole desktop, frozen.
pub struct Shot {
    pub regions: Vec<Region>,
}

/// What the desktop reads as where no monitor covers it.
///
/// Two monitors of different heights, or set at an angle to each other, leave gaps in the
/// bounding box that no capture covers. Opaque black rather than transparent, for the same
/// reason the PNG carries no alpha: a hole in a screenshot is worse than a black corner, and
/// this is at least what the space between two monitors looks like.
const NO_MONITOR: u32 = 0xff00_0000;

impl Shot {
    /// The bounding box of every monitor, in global coordinates.
    ///
    /// This is what "the whole desktop" means, and it is not always the sum of the monitors:
    /// two set side by side at different heights leave gaps, which read as [`NO_MONITOR`].
    pub fn bounds(&self) -> Rect {
        let Some(first) = self.regions.first() else {
            return Rect::new(0, 0, 0, 0);
        };
        let mut left = first.rect.left();
        let mut top = first.rect.top();
        let mut right = first.rect.right();
        let mut bottom = first.rect.bottom();
        for region in &self.regions[1..] {
            left = left.min(region.rect.left());
            top = top.min(region.rect.top());
            right = right.max(region.rect.right());
            bottom = bottom.max(region.rect.bottom());
        }
        Rect::new(left, top, right - left, bottom - top)
    }

    /// One pixel of the frozen desktop, by global coordinate.
    ///
    /// The **last** region covering the point wins. Monitors do not normally overlap, but a
    /// mirrored or misconfigured layout can put two on the same coordinates, and picking
    /// deterministically beats whichever the iteration happened to reach first.
    pub fn pixel(&self, x: i32, y: i32) -> u32 {
        self.regions
            .iter()
            .rev()
            .find_map(|region| region.at(x, y))
            .unwrap_or(NO_MONITOR)
    }

    /// Copy a rectangle out as RGB rows, ready for [`encode_png`].
    ///
    /// Clipped to nothing gives an empty image rather than an error: a caller asking for a
    /// zero-width selection has made a UI mistake, not a fatal one.
    pub fn crop(&self, rect: Rect) -> Image {
        let (w, h) = (rect.w.max(0), rect.h.max(0));
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for y in rect.top()..rect.top() + h {
            for x in rect.left()..rect.left() + w {
                let pixel = self.pixel(x, y);
                // 0xAARRGGBB, alpha discarded -- see the module note.
                rgb.push((pixel >> 16) as u8);
                rgb.push((pixel >> 8) as u8);
                rgb.push(pixel as u8);
            }
        }
        Image {
            width: w as u32,
            height: h as u32,
            rgb,
        }
    }
}

/// A cropped rectangle as packed 8-bit RGB, which is what PNG wants.
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// `width * height` pixels, three bytes each.
    pub rgb: Vec<u8>,
}

impl Image {
    /// Encode as a PNG.
    ///
    /// In memory rather than to a path, because the same bytes go to a file *or* to the
    /// clipboard, and the clipboard owner is a forked child that must already hold them --
    /// it cannot go back to the compositor for a frame after this process is gone.
    pub fn encode_png(&self) -> Result<Vec<u8>, String> {
        if self.width == 0 || self.height == 0 {
            return Err("the selection is empty".to_string());
        }
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|err| format!("could not write the PNG header: {err}"))?;
            writer
                .write_image_data(&self.rgb)
                .map_err(|err| format!("could not write the PNG: {err}"))?;
            writer
                .finish()
                .map_err(|err| format!("could not finish the PNG: {err}"))?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monitor filled with one color, at a given place in the layout.
    fn region(x: i32, y: i32, w: i32, h: i32, color: u32) -> Region {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            pixels.extend_from_slice(&color.to_ne_bytes());
        }
        Region {
            rect: Rect::new(x, y, w, h),
            pixels,
        }
    }

    #[test]
    fn one_monitor_bounds_itself() {
        let shot = Shot {
            regions: vec![region(0, 0, 4, 3, 0xff11_2233)],
        };
        assert_eq!(shot.bounds(), Rect::new(0, 0, 4, 3));
        assert_eq!(shot.pixel(3, 2), 0xff11_2233);
    }

    /// Two monitors side by side are one desktop, and the second's pixels are reached at its
    /// own global coordinates rather than at zero.
    #[test]
    fn two_monitors_make_one_desktop() {
        let shot = Shot {
            regions: vec![
                region(0, 0, 4, 4, 0xffaa_0000),
                region(4, 0, 4, 4, 0xff00_bb00),
            ],
        };
        assert_eq!(shot.bounds(), Rect::new(0, 0, 8, 4));
        assert_eq!(shot.pixel(0, 0), 0xffaa_0000);
        assert_eq!(shot.pixel(4, 0), 0xff00_bb00);
        assert_eq!(shot.pixel(7, 3), 0xff00_bb00);
    }

    /// The whole reason global coordinates are used: a selection may straddle the seam.
    #[test]
    fn a_crop_can_span_two_monitors() {
        let shot = Shot {
            regions: vec![
                region(0, 0, 4, 4, 0xffaa_0000),
                region(4, 0, 4, 4, 0xff00_bb00),
            ],
        };
        let image = shot.crop(Rect::new(2, 1, 4, 2));
        assert_eq!((image.width, image.height), (4, 2));
        // Left half is the first monitor's red, right half the second's green.
        assert_eq!(&image.rgb[0..3], &[0xaa, 0x00, 0x00]);
        assert_eq!(&image.rgb[6..9], &[0x00, 0xbb, 0x00]);
    }

    /// Monitors at different heights leave a gap in the bounding box, and a crop over it must
    /// produce something rather than run off the end of a buffer.
    #[test]
    fn a_gap_between_monitors_reads_as_black() {
        let shot = Shot {
            regions: vec![
                region(0, 0, 2, 4, 0xffaa_0000),
                region(2, 2, 2, 2, 0xff00_bb00),
            ],
        };
        assert_eq!(shot.bounds(), Rect::new(0, 0, 4, 4));
        // Top-right is covered by neither.
        assert_eq!(shot.pixel(3, 0), NO_MONITOR);
        let image = shot.crop(shot.bounds());
        assert_eq!(image.rgb.len(), 4 * 4 * 3);
    }

    #[test]
    fn a_crop_outside_every_monitor_is_still_an_image() {
        let shot = Shot {
            regions: vec![region(0, 0, 2, 2, 0xffff_ffff)],
        };
        let image = shot.crop(Rect::new(100, 100, 2, 2));
        assert_eq!(image.rgb, vec![0; 2 * 2 * 3]);
    }

    #[test]
    fn an_empty_selection_will_not_encode() {
        let shot = Shot {
            regions: vec![region(0, 0, 2, 2, 0xffff_ffff)],
        };
        assert!(shot.crop(Rect::new(0, 0, 0, 5)).encode_png().is_err());
    }

    /// The bytes really are a PNG, and the alpha channel really is gone.
    #[test]
    fn a_crop_encodes_to_a_png_without_alpha() {
        let shot = Shot {
            regions: vec![region(0, 0, 2, 2, 0x0012_3456)],
        };
        let png = shot.crop(Rect::new(0, 0, 2, 2)).encode_png().unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().unwrap();
        assert_eq!(reader.info().color_type, png::ColorType::Rgb);
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        // The undefined 0x00 alpha byte of an Xrgb pixel never reaches the file.
        assert_eq!(&buf[..info.buffer_size()][..3], &[0x12, 0x34, 0x56]);
    }
}
