// SPDX-License-Identifier: GPL-3.0-or-later
//! `screenshot.toml`, and the little of this that is worth configuring.
//!
//! ```toml
//! # ~/.config/wlrix/screenshot.toml
//! [save]
//! dir = "~/Pictures/Screenshots"          # default: the XDG pictures dir + /Screenshots
//! filename = "Screenshot_%Y-%m-%d_%H-%M-%S"   # strftime, without the extension
//!
//! [capture]
//! cursor = false          # draw the pointer into the shot
//!
//! [appearance]
//! palette = "gotham"      # the colour scheme; default is "classic"
//! dim = 0.55              # how far the unselected area is darkened, 0.0 to 1.0
//! ```
//!
//! Read from the user's config directory first, then `/etc/wlrix`; the first file found wins
//! outright rather than merging, so what a user sees in their own file is the whole of what
//! they get. One shape of file across the stack.
//!
//! Unknown keys are an error, as everywhere in wlRIX -- a silently ignored typo in a config
//! file is a bad afternoon, and the cost of being strict is a clear message instead.

use serde::Deserialize;

/// The default filename template. `strftime`, so `%Y-%m-%d_%H-%M-%S` is what it looks like.
const DEFAULT_FILENAME: &str = "Screenshot_%Y-%m-%d_%H-%M-%S";
/// The subdirectory of the pictures directory a shot lands in, when nothing says otherwise.
const DEFAULT_SUBDIR: &str = "Screenshots";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub save: Save,
    pub capture: Capture,
    pub appearance: Appearance,
}

/// Where a saved shot goes and what it is called.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Save {
    /// The directory, with a leading `~/` expanded. Empty means the XDG pictures directory
    /// plus `Screenshots`, worked out at save time rather than here -- this type is parsed by
    /// `--check-config` in contexts with no `$HOME` worth consulting.
    pub dir: String,
    /// The base name, as a `strftime` template and without an extension.
    ///
    /// Passed to the C library, so every conversion `strftime(3)` documents works. The result
    /// has path separators replaced before it is used; see [`crate::save`].
    pub filename: String,
}

impl Default for Save {
    fn default() -> Self {
        Self {
            dir: String::new(),
            filename: DEFAULT_FILENAME.to_string(),
        }
    }
}

impl Save {
    /// The configured directory, or the XDG pictures directory plus `Screenshots`.
    pub fn directory(&self) -> Option<std::path::PathBuf> {
        if !self.dir.trim().is_empty() {
            return Some(crate::xdg::expand_tilde(self.dir.trim()));
        }
        crate::xdg::pictures_dir().map(|dir| dir.join(DEFAULT_SUBDIR))
    }
}

/// What goes into the picture.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Capture {
    /// Draw the pointer into the shot.
    ///
    /// Off by default: the pointer is almost always somewhere incidental at the moment the key
    /// is pressed, and a shot of a menu is spoiled rather than explained by an arrow in the
    /// corner of it. The compositor supports it either way -- it is `PaintCursors` on the
    /// capture session.
    pub cursor: bool,
}

/// How the overlay looks.
///
/// The section is named for the compositor's and `wlrix-desktop`'s, so the files read alike.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Appearance {
    /// A scheme id from `wlrix-ui`: `classic`, `classic-g10`, `classic-g24`, `gotham`.
    /// Absent, empty or unrecognized means the default, with a line on stderr for the last of
    /// those -- a mistyped scheme name must not leave the overlay unpainted.
    pub palette: Option<String>,
    /// How far the area outside the selection is darkened, `0.0` (not at all) to `1.0` (black).
    ///
    /// Clamped rather than rejected: a silly number here should cost the look, not the
    /// screenshot, and there is no value in the range that stops the tool working.
    pub dim: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            palette: None,
            // Dark enough that the selection reads as the subject, light enough that what is
            // outside it is still recognizable -- which matters, because the whole point of
            // adjusting a selection is seeing what you are about to leave out.
            dim: 0.55,
        }
    }
}

impl Appearance {
    /// The dim as the 0-255 blend amount [`wlrix_ui::color::Rgb::blend`] takes.
    pub fn dim_amount(&self) -> u8 {
        (self.dim.clamp(0.0, 1.0) * 255.0).round() as u8
    }
}

/// Parse a candidate config file, for `--check-config`.
///
/// This program's own serde types are the authority on what `screenshot.toml` may contain.
/// `wlrix-settings-daemon` writes a temporary file and runs this against it before renaming it
/// into place, so a settings app cannot produce a file this program would refuse -- which
/// matters because `deny_unknown_fields` means one wrong key costs the *whole* file and the
/// user silently gets built-in defaults for all of it.
///
/// Deliberately not [`Config::load`]: that reports and carries on with defaults, which is right
/// at startup -- a typo should cost the setting, not the screenshot -- and exactly wrong here,
/// where the question *is* whether the file is acceptable.
pub fn check(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    toml::from_str::<Config>(&text)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

impl Config {
    /// Load the first config file that exists. No file at all is not an error -- the defaults
    /// are what everything was tuned with.
    pub fn load() -> Self {
        for path in crate::xdg::config_paths() {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                // Not-found is the ordinary case; only real errors are worth a line.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    eprintln!("wlrix-screenshot: could not read {}: {err}", path.display());
                    continue;
                }
            };
            match toml::from_str::<Self>(&text) {
                Ok(config) => return config,
                Err(err) => {
                    // Loud, then carry on: a broken config should not cost the user the
                    // screenshot they just asked for and cannot take again.
                    eprintln!("wlrix-screenshot: {} is not valid: {err}", path.display());
                    return Self::default();
                }
            }
        }
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_all_defaults() {
        let config: Config = toml::from_str("").expect("empty config should parse");
        assert_eq!(config.save.filename, DEFAULT_FILENAME);
        assert!(!config.capture.cursor);
        assert_eq!(config.appearance.palette, None);
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        // `cursour` would otherwise silently do nothing, and the user would never find out
        // why the pointer was missing from every shot.
        assert!(toml::from_str::<Config>("[capture]\ncursour = true\n").is_err());
        assert!(toml::from_str::<Config>("[savee]\ndir = \"/tmp\"\n").is_err());
    }

    #[test]
    fn one_key_leaves_the_rest_alone() {
        let config: Config = toml::from_str("[capture]\ncursor = true\n").unwrap();
        assert!(config.capture.cursor);
        assert_eq!(config.save.filename, DEFAULT_FILENAME);
    }

    /// A number outside the range costs the look, not the screenshot.
    #[test]
    fn the_dim_is_clamped_rather_than_refused() {
        let over: Config = toml::from_str("[appearance]\ndim = 4.0\n").unwrap();
        assert_eq!(over.appearance.dim_amount(), 255);
        let under: Config = toml::from_str("[appearance]\ndim = -1.0\n").unwrap();
        assert_eq!(under.appearance.dim_amount(), 0);
    }
}
