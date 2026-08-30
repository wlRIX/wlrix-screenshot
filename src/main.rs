// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX screenshots.
//!
//! Freezes the screen, pulls out a region that can be adjusted, and saves it or copies it.
//! Normally reached by a key rather than by name: `wlrix-compositor` binds Print to a region,
//! Alt+Print to the active window and Shift+Print to the whole desktop, and `--select` is how
//! the compositor describes the active window -- it knows which one it is and owns the frame
//! geometry, which no client can work out for itself.
//!
//! Also the helper `xdg-desktop-portal-wlrix` spawns for
//! `org.freedesktop.impl.portal.Screenshot`; see [`wlrix_screenshot::portal`].

use wlrix_screenshot::{clipboard, config::Config, grab, portal, save, shot::Shot, ui};
use wlrix_ui::canvas::Rect;

/// What the process tells whoever started it.
///
/// The same three answers the picker gives `xdg-desktop-portal-wlrix`, and for the same reason:
/// a caller has to be able to tell "the user said no" from "this went wrong", because showing
/// an error for a canceled screenshot is a bug users notice.
const EXIT_CANCELED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_FAILED: u8 = 3;

/// Which rectangle the overlay starts with.
enum Mode {
    /// Nothing selected: the user drags one out.
    Region,
    /// The whole desktop.
    All,
    /// A rectangle somebody else worked out, in global desktop coordinates.
    Preset(Rect),
}

struct Args {
    mode: Mode,
    /// Whether to put the overlay up at all.
    interactive: bool,
    /// Save when finished, and where. `None` means the configured directory.
    save: bool,
    output: Option<std::path::PathBuf>,
    /// Copy when finished.
    copy: bool,
    /// Answer on stdout as the portal expects. See [`portal`].
    portal: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::Region,
            interactive: true,
            save: false,
            output: None,
            copy: false,
            portal: false,
        }
    }
}

const HELP: &str = "\
wlrix-screenshot {version}

Take a screenshot on the wlRIX desktop. Freezes the screen and pulls out a
region you can adjust, then saves it or copies it.

Usage: wlrix-screenshot [options]

Options:
  --all                  start with the whole desktop selected
  --select X,Y,W,H       start with that rectangle selected, in desktop
                         coordinates. This is how wlrix-compositor describes
                         the active window for Alt+Print.
  --output <path>        where a save goes; default is the configured
                         directory, ~/Pictures/Screenshots
  --copy                 no overlay: capture, copy to the clipboard, exit
  --no-ui                no overlay: capture, save, exit
  --portal               read a JSON request on stdin and answer on stdout;
                         for xdg-desktop-portal-wlrix, not for people
  --check-config <path>  say whether that file would be accepted as
                         screenshot.toml, and exit
  -V, --version          version, then exit
  -h, --help             this message

In the overlay:
  drag                   pull out a selection; drag a handle to resize it,
                         or its middle to move it
  arrows                 nudge by one pixel; Shift for ten, Ctrl to resize
  Ctrl+A                 select the whole desktop
  Enter                  save          Ctrl+C  copy          Esc  cancel

Exit: 0 taken, 1 canceled, 2 bad arguments, 3 failed.

Settings live in ~/.config/wlrix/screenshot.toml.";

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1);
    // `while let` rather than a `for`, so an option can take the argument after it.
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--all" => args.mode = Mode::All,
            "--select" => {
                let value = argv.next().ok_or("--select needs X,Y,W,H")?;
                args.mode = Mode::Preset(parse_rect(&value)?);
            }
            "--output" => {
                args.output = Some(argv.next().ok_or("--output needs a path")?.into());
                args.save = true;
            }
            "--copy" => {
                args.copy = true;
                args.interactive = false;
            }
            "--no-ui" => {
                args.interactive = false;
                args.save = true;
            }
            "--portal" => args.portal = true,
            "--version" | "-V" => {
                println!("wlrix-screenshot {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--help" | "-h" => {
                println!("{}", HELP.replace("{version}", env!("CARGO_PKG_VERSION")));
                return Ok(None);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    // With the overlay up, the buttons decide -- but a Save still has to land somewhere, and
    // `--output` is what says where.
    if args.interactive {
        args.save = false;
        args.copy = false;
    }
    Ok(Some(args))
}

/// `X,Y,W,H`, as the compositor writes it.
fn parse_rect(text: &str) -> Result<Rect, String> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let [x, y, w, h] = parts.as_slice() else {
        return Err(format!("{text:?} is not X,Y,W,H"));
    };
    let number = |value: &str| {
        value
            .parse::<i32>()
            .map_err(|_| format!("{value:?} is not a number (in {text:?})"))
    };
    let (w, h) = (number(w)?, number(h)?);
    if w <= 0 || h <= 0 {
        return Err(format!("{text:?} has no area"));
    }
    Ok(Rect::new(number(x)?, number(y)?, w, h))
}

fn main() -> std::process::ExitCode {
    // Before anything else, and before the logger there is not: this mode reads an image on
    // stdin and never touches a font, a config file or an overlay. See `clipboard`.
    if std::env::args().nth(1).as_deref() == Some(clipboard::SERVE_ARG) {
        return match clipboard::serve() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("wlrix-screenshot: {err}");
                std::process::ExitCode::from(EXIT_FAILED)
            }
        };
    }

    // Answers a question about a file rather than doing anything with it, so it starts nothing
    // and needs no compositor. `wlrix-settings-daemon` runs this against a candidate file
    // before renaming it into place, which is what stops a settings app from writing a
    // `screenshot.toml` this program would refuse.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("--check-config") {
        let Some(path) = argv.get(1) else {
            eprintln!("wlrix-screenshot: --check-config needs a path");
            return std::process::ExitCode::from(EXIT_USAGE);
        };
        return match wlrix_screenshot::config::check(std::path::Path::new(path)) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("{why}");
                std::process::ExitCode::from(EXIT_FAILED)
            }
        };
    }

    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("wlrix-screenshot: {err}");
            eprintln!("try --help");
            return std::process::ExitCode::from(EXIT_USAGE);
        }
    };

    match run(args) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::from(EXIT_CANCELED),
        Err(err) => {
            eprintln!("wlrix-screenshot: {err}");
            std::process::ExitCode::from(EXIT_FAILED)
        }
    }
}

/// The whole job. `false` means the user canceled, which is not a failure.
fn run(mut args: Args) -> Result<bool, String> {
    // The portal's request replaces whatever the command line said, apart from `--portal`
    // itself -- it is the caller, and the two are never combined by anything but a mistake.
    let request = args.portal.then(portal::read_manifest).transpose()?;
    if let Some(request) = &request {
        args.mode = match request.target {
            portal::TARGET_SCREEN => Mode::All,
            // Area, and anything unrecognized. An unknown target is a newer frontend asking
            // for something this backend did not advertise; letting the user draw a rectangle
            // is the answer closest to every target there is.
            _ => Mode::Region,
        };
        args.interactive = request.interactive || request.target == portal::TARGET_AREA;
        args.output = Some(request.path.clone().into());
        args.save = true;
        args.copy = false;
    }

    let config = Config::load();
    let cursor = config.capture.cursor || request.as_ref().is_some_and(|r| r.cursor);

    let mut client = ui::connect(&config)?;
    // Before anything is mapped: the overlay covers the desktop, so capturing afterwards would
    // photograph the dialog asking about the picture.
    let shot = grab::grab(
        &mut client.app,
        &mut client.queue,
        &client.connection,
        cursor,
    )?;
    let bounds = shot.bounds();

    let preset = match args.mode {
        Mode::Region => None,
        Mode::All => Some(bounds),
        Mode::Preset(rect) => Some(rect),
    };

    let (rect, save, copy) = if args.interactive {
        match ui::run(client, shot.clone_ref(), preset)? {
            ui::Choice::Save(rect) => (rect, true, false),
            ui::Choice::Copy(rect) => (rect, false, true),
            ui::Choice::Canceled => return Ok(false),
        }
    } else {
        // No overlay, so nothing narrowed the shot: the preset, or everything.
        (preset.unwrap_or(bounds), args.save, args.copy)
    };

    let image = shot.crop(rect);
    let png = image.encode_png()?;

    if copy {
        clipboard::copy(&png)?;
    }
    let mut written = None;
    if save {
        let path = match &args.output {
            Some(path) => path.clone(),
            None => save::destination(&config.save)?,
        };
        save::write(&path, &png)?;
        written = Some(path);
    }

    match (&request, &written) {
        (Some(_), Some(path)) => portal::answer(&path.to_string_lossy())?,
        // A portal request that produced no file is a bug in this program, not a cancel: the
        // manifest always names a path and always asks for it to be written.
        (Some(_), None) => return Err("the portal asked for a file and none was written".into()),
        (None, Some(path)) => eprintln!("wlrix-screenshot: saved {}", path.display()),
        (None, None) => {}
    }
    Ok(true)
}

/// The frozen desktop is needed after the overlay has consumed the client.
///
/// [`ui::run`] takes ownership of the shot -- it draws from it on every pointer move -- so the
/// cropping afterwards needs its own copy of the pixels. A screen's worth is a few megabytes
/// and this happens once, at the end, which is a far better trade than reference-counting the
/// buffers through the whole overlay.
trait CloneRef {
    fn clone_ref(&self) -> Shot;
}

impl CloneRef for Shot {
    fn clone_ref(&self) -> Shot {
        Shot {
            regions: self
                .regions
                .iter()
                .map(|region| wlrix_screenshot::shot::Region {
                    rect: region.rect,
                    pixels: region.pixels.clone(),
                })
                .collect(),
        }
    }
}
