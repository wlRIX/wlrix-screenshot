// SPDX-License-Identifier: GPL-3.0-or-later
//! The contract with `xdg-desktop-portal-wlrix`.
//!
//! The portal backend implements `org.freedesktop.impl.portal.Screenshot` by spawning this
//! program, exactly as it already spawns `wlrix-source-picker` to ask which screen to share.
//! One shape of helper, one shape of contract:
//!
//! - A JSON manifest on **stdin**, closed straight after.
//! - The answer as JSON on **stdout**.
//! - The **exit code** says which happened: `0` taken, `1` canceled, anything else failed.
//!   Both are checked, so a helper that dies mid-answer produces neither valid stdout nor a
//!   zero exit and cannot be mistaken for a screenshot that happened.
//!
//! The portal names the file rather than letting this program choose. A portal screenshot is
//! not one the user asked to keep -- the frontend copies it into the requesting application's
//! document store -- so it belongs in `$XDG_RUNTIME_DIR` and not in anybody's Pictures folder.

use serde::{Deserialize, Serialize};

/// `org.freedesktop.impl.portal.Screenshot`'s target values.
///
/// A single value, not a bitmask, and only the two the portal advertises ever arrive here.
/// `Window` (2) and `ActiveWindow` (8) are deliberately not offered: a window shot needs the
/// compositor's frame rectangle, which only reaches this program through `--select` on a
/// keybind, and there is no window picker yet.
pub const TARGET_SCREEN: u32 = 1;
pub const TARGET_AREA: u32 = 4;

/// What the portal asks for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The application that asked, for the overlay's wording. Empty for an unsandboxed
    /// application, which is most of them.
    #[serde(default)]
    pub app_id: String,
    /// Whether the user gets to choose an area. The portal passes the application's
    /// `interactive` hint through, defaulting to no.
    #[serde(default)]
    pub interactive: bool,
    /// One of the constants above.
    #[serde(default)]
    pub target: u32,
    #[serde(default)]
    pub cursor: bool,
    /// Where to put the file. Chosen by the portal; see the module note.
    pub path: String,
}

/// What this program answers.
#[derive(Debug, Serialize)]
pub struct Answer<'a> {
    pub path: &'a str,
}

/// Read the manifest from stdin.
pub fn read_manifest() -> Result<Manifest, String> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
        .map_err(|err| format!("could not read the portal manifest: {err}"))?;
    serde_json::from_str(&text).map_err(|err| format!("the portal manifest is not valid: {err}"))
}

/// Answer on stdout.
pub fn answer(path: &str) -> Result<(), String> {
    let json = serde_json::to_string(&Answer { path })
        .map_err(|err| format!("could not encode the answer: {err}"))?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_needs_only_a_path() {
        let manifest: Manifest = serde_json::from_str(r#"{"path":"/run/x.png"}"#).unwrap();
        assert_eq!(manifest.path, "/run/x.png");
        assert!(!manifest.interactive);
        assert_eq!(manifest.target, 0);
    }

    #[test]
    fn a_full_manifest_parses() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"app_id":"org.gnome.Screenshot","interactive":true,"target":4,
                "cursor":false,"path":"/run/user/1000/wlrix-portal/screenshot-3.png"}"#,
        )
        .unwrap();
        assert!(manifest.interactive);
        assert_eq!(manifest.target, TARGET_AREA);
        assert_eq!(manifest.app_id, "org.gnome.Screenshot");
    }

    /// The manifest comes from another process, and a key this program does not understand
    /// means the two halves disagree about the contract -- which is worth an error rather than
    /// a screenshot taken under the wrong terms.
    #[test]
    fn an_unknown_key_is_refused() {
        assert!(serde_json::from_str::<Manifest>(r#"{"path":"/x","modal":true}"#).is_err());
    }

    #[test]
    fn a_manifest_without_a_path_is_refused() {
        assert!(serde_json::from_str::<Manifest>(r#"{"target":1}"#).is_err());
    }
}
