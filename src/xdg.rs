// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding where a screenshot goes, and the settings file that says so.
//!
//! Ported from `wlrix-desktop/src/xdg.rs`, whose `parse_user_dirs` is already generic on the
//! key it looks for -- this asks it for `XDG_PICTURES_DIR` instead of `XDG_DESKTOP_DIR`. The
//! reason that file exists is the same here:
//!
//! `XDG_PICTURES_DIR` is not an environment variable in practice. `xdg-user-dirs` writes it
//! into `~/.config/user-dirs.dirs`, a file of shell assignments that a login script sources,
//! and a Wayland client spawned from a keybind never runs that script. So the file has to be
//! read directly. The environment is still checked first, since someone setting it by hand
//! means it.

use std::path::{Path, PathBuf};

/// Where `xdg-user-dirs` records the user's directory choices.
const USER_DIRS: &str = "user-dirs.dirs";
/// The settings file, relative to a config directory.
pub const CONFIG_NAME: &str = "wlrix/screenshot.toml";
/// Consulted when the user has no config of their own, as every other component does.
const SYSTEM_CONFIG_DIR: &str = "/etc";

/// `$HOME`, or `None` when even that is unset.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME`, or `~/.config` as the spec says to assume.
pub fn user_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    home().map(|home| home.join(".config"))
}

/// Where to look for the settings file, most specific first.
pub fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = user_config_dir() {
        paths.push(dir.join(CONFIG_NAME));
    }
    paths.push(Path::new(SYSTEM_CONFIG_DIR).join(CONFIG_NAME));
    paths
}

/// The per-user runtime directory.
///
/// Required rather than falling back to `/tmp`, for the reason
/// `xdg-desktop-portal-wlrix/src/preview.rs` gives: it is owned by one user and cleaned up at
/// logout, where a predictable path in a world-writable directory is somewhere another user can
/// leave a symlink. Only the portal path needs this; an ordinary save goes to Pictures.
pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// The user's pictures directory.
///
/// `$XDG_PICTURES_DIR` wins if it is set, then `user-dirs.dirs`, then `~/Pictures`. The result
/// is not required to exist -- [`crate::save`] creates it, since a machine where nobody has
/// taken a screenshot yet is the ordinary case rather than an error.
pub fn pictures_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_PICTURES_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }

    let home = home()?;
    let from_file = user_config_dir()
        .map(|dir| dir.join(USER_DIRS))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| parse_user_dirs(&text, "XDG_PICTURES_DIR", &home));

    Some(from_file.unwrap_or_else(|| home.join("Pictures")))
}

/// Expand a leading `~/` against `$HOME`.
///
/// Config files are hand-edited, and a person writing a path in one writes `~/Pictures`. The
/// shell is not involved by the time this program reads it, so nothing else would.
pub fn expand_tilde(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), home()) {
        (Some(rest), Some(home)) => home.join(rest),
        // A bare `~` is a home directory too, and `~other/` is a shell feature this does not
        // pretend to have -- it is taken literally, which is at least visible.
        (None, Some(home)) if path == "~" => home,
        _ => PathBuf::from(path),
    }
}

/// Pull one directory out of a `user-dirs.dirs` file.
///
/// The format is shell assignments, e.g. `XDG_PICTURES_DIR="$HOME/画像"`. Only the two forms
/// `xdg-user-dirs` actually writes are handled -- a `$HOME`-relative path and an absolute one --
/// rather than pretending to be a shell. A line that is neither is skipped, which falls back to
/// `~/Pictures` rather than pointing at somewhere wrong.
fn parse_user_dirs(text: &str, key: &str, home: &Path) -> Option<PathBuf> {
    // Last assignment wins, as it would if a shell sourced the file.
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let value = line
                .strip_prefix(key)?
                .trim_start()
                .strip_prefix('=')?
                .trim();
            // Unquote. `xdg-user-dirs` always quotes, but an unquoted value is still a value
            // and is cheap to accept.
            let value = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(value);
            if value.is_empty() {
                return None;
            }
            if let Some(rest) = value.strip_prefix("$HOME/") {
                return Some(home.join(rest));
            }
            if value == "$HOME" {
                return Some(home.to_path_buf());
            }
            value.starts_with('/').then(|| PathBuf::from(value))
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    #[test]
    fn home_relative_paths_are_expanded() {
        let text = "XDG_PICTURES_DIR=\"$HOME/Pictures\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_PICTURES_DIR", &home()),
            Some(PathBuf::from("/home/tester/Pictures"))
        );
    }

    #[test]
    fn absolute_paths_are_taken_as_they_are() {
        let text = "XDG_PICTURES_DIR=\"/srv/shared/shots\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_PICTURES_DIR", &home()),
            Some(PathBuf::from("/srv/shared/shots"))
        );
    }

    #[test]
    fn a_localized_directory_name_survives() {
        // The dev machine runs a Japanese locale, where xdg-user-dirs writes exactly this.
        let text = "XDG_PICTURES_DIR=\"$HOME/画像\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_PICTURES_DIR", &home()),
            Some(PathBuf::from("/home/tester/画像"))
        );
    }

    #[test]
    fn other_keys_and_comments_are_ignored() {
        let text = "# Created by xdg-user-dirs-update\n\
                    XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n\
                    XDG_PICTURES_DIR=\"$HOME/Pictures\"\n\
                    XDG_MUSIC_DIR=\"$HOME/Music\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_PICTURES_DIR", &home()),
            Some(PathBuf::from("/home/tester/Pictures"))
        );
    }

    #[test]
    fn the_last_assignment_wins() {
        let text = "XDG_PICTURES_DIR=\"$HOME/first\"\nXDG_PICTURES_DIR=\"$HOME/second\"\n";
        assert_eq!(
            parse_user_dirs(text, "XDG_PICTURES_DIR", &home()),
            Some(PathBuf::from("/home/tester/second"))
        );
    }

    #[test]
    fn a_value_we_cannot_read_is_no_answer_at_all() {
        // Anything needing real shell expansion is refused rather than guessed at, so the
        // caller falls back to ~/Pictures instead of saving somewhere wrong.
        for text in [
            "XDG_PICTURES_DIR=\"${HOME}/Pictures\"\n",
            "XDG_PICTURES_DIR=\"$OTHER/Pictures\"\n",
            "XDG_PICTURES_DIR=\"\"\n",
            "XDG_PICTURES_DIR=\"relative/path\"\n",
        ] {
            assert_eq!(
                parse_user_dirs(text, "XDG_PICTURES_DIR", &home()),
                None,
                "{text}"
            );
        }
    }
}
