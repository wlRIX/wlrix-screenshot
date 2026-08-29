// SPDX-License-Identifier: GPL-3.0-or-later
//! Naming a screenshot and putting it on disk.
//!
//! ## Why `strftime` from libc rather than a date crate
//!
//! The name wants to be local time, and local time means the `TZ` database -- which the C
//! library already has open and a Rust date crate would either pull in again or get wrong
//! around a DST boundary. `libc` is already a dependency for [`crate::clipboard`]'s `fork`, so
//! `localtime_r` plus `strftime` costs nothing and gives the config file every conversion
//! `strftime(3)` documents rather than a handful this program invented.

use std::ffi::CString;
use std::path::{Path, PathBuf};

/// The extension, and the only format written. There is no choice to configure yet, so there
/// is no key for one.
const EXTENSION: &str = "png";
/// How long a formatted name may be. Generous -- the default is thirty characters -- but
/// `strftime` needs a bound, and a template that expands past this is a mistake worth naming.
const NAME_MAX: usize = 512;

/// Where a shot would be saved, creating the directory if it is not there.
///
/// The directory is created rather than required: a machine where nobody has taken a
/// screenshot yet has no `~/Pictures/Screenshots`, and that is the ordinary case rather than
/// something to fail on.
pub fn destination(config: &crate::config::Save) -> Result<PathBuf, String> {
    let dir = config
        .directory()
        .ok_or("no place to save: neither the config file nor $HOME says where")?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    Ok(unique(&dir, &format_name(&config.filename)?))
}

/// Write the bytes, and answer with where they went.
///
/// The parent directory is created here as well as in [`destination`], because `--output`
/// names a path this program never chose -- and `wlrix-screenshot --output ~/new/dir/x.png`
/// failing on the directory rather than making it would be a poor way to find that out.
pub fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|err| format!("could not write {}: {err}", path.display()))
}

/// Expand a `strftime` template against the local time now.
fn format_name(template: &str) -> Result<String, String> {
    let template = CString::new(template)
        .map_err(|_| "the filename template contains a NUL byte".to_string())?;

    // SAFETY: `time` accepts a null pointer and returns the value; `localtime_r` fills a `tm`
    // this stack frame owns, and is the reentrant form precisely so it does not hand back a
    // pointer into shared state. `strftime` writes at most `buffer.len()` bytes including the
    // terminator, and reports how many it wrote.
    let formatted = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return Err("the system clock could not be read as a local time".to_string());
        }
        let mut buffer = [0u8; NAME_MAX];
        let written = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            template.as_ptr(),
            &tm,
        );
        if written == 0 {
            return Err(
                "the filename template expanded to nothing, or to more than this program \
                 will use as a name"
                    .to_string(),
            );
        }
        String::from_utf8_lossy(&buffer[..written]).into_owned()
    };

    Ok(sanitize(&formatted))
}

/// Make a formatted name safe to be one path component.
///
/// The template is checked at config time, but its *expansion* is not the same string: `%x` is
/// a locale's date and in plenty of locales that contains slashes, which would silently turn
/// one name into a directory path. Anything not plainly safe becomes an underscore.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    // A name that is nothing but separators, or the two directory entries that are not names.
    match cleaned.trim() {
        "" | "." | ".." => "Screenshot".to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// `dir/base.png`, or `dir/base-2.png` and upward if that exists.
///
/// Two screenshots inside one second is a real thing -- holding Print down does it -- and
/// silently overwriting the first would lose it. Counting stops at a bound rather than looping
/// forever against a directory nothing can be written to.
fn unique(dir: &Path, base: &str) -> PathBuf {
    let first = dir.join(format!("{base}.{EXTENSION}"));
    if !first.exists() {
        return first;
    }
    for n in 2..1000 {
        let candidate = dir.join(format!("{base}-{n}.{EXTENSION}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // A thousand shots in one second is not a case worth more code than this. The last
    // candidate is returned and overwritten, which is visible, rather than failing the save.
    dir.join(format!("{base}-1000.{EXTENSION}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_expands_to_something_that_looks_like_a_date() {
        let name = format_name("Screenshot_%Y-%m-%d").expect("the default template must work");
        assert!(name.starts_with("Screenshot_20"), "{name}");
        // Exactly `Screenshot_` plus `YYYY-MM-DD`.
        assert_eq!(name.len(), "Screenshot_".len() + 10, "{name}");
    }

    #[test]
    fn a_literal_template_comes_back_unchanged() {
        assert_eq!(format_name("shot").unwrap(), "shot");
    }

    /// The trap this exists for: `%x` is a locale's date, and several locales put slashes in
    /// it. Without sanitizing, one name would silently become a path.
    #[test]
    fn separators_in_an_expansion_are_not_path_components() {
        assert_eq!(sanitize("2026/08/27"), "2026_08_27");
        assert_eq!(sanitize("a\0b"), "a_b");
    }

    #[test]
    fn a_name_that_is_not_a_name_falls_back() {
        assert_eq!(sanitize(""), "Screenshot");
        assert_eq!(sanitize("   "), "Screenshot");
        assert_eq!(sanitize(".."), "Screenshot");
    }

    #[test]
    fn a_template_with_a_nul_is_refused_rather_than_truncated() {
        assert!(format_name("shot\0extra").is_err());
    }

    /// Holding the key down takes several shots inside one second, and they must not be the
    /// same file.
    #[test]
    fn a_second_shot_in_the_same_second_gets_its_own_name() {
        let dir =
            std::env::temp_dir().join(format!("wlrix-screenshot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let first = unique(&dir, "shot");
        assert_eq!(first, dir.join("shot.png"));
        std::fs::write(&first, b"").unwrap();

        let second = unique(&dir, "shot");
        assert_eq!(second, dir.join("shot-2.png"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
