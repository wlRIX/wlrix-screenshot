# wlrix-screenshot

Screenshots for the wlRIX desktop. Freezes the screen, pulls out a region you can adjust, and saves it or copies it —
after IRIX's own habits and KDE's Spectacle, drawn in 4Dwm chrome.

- **Language:** Rust
- **License:** GPL-3.0-or-later

Reached by a key rather than by name. `wlrix-compositor` binds:

|               |                                  |
|---------------|----------------------------------|
| `Print`       | drag a region out                |
| `Alt+Print`   | the active window, frame and all |
| `Shift+Print` | the whole desktop                |

All three land in the same overlay with the selection already set (or not, for `Print`), so there is one thing to learn
rather than three.

## Why Rust, and not an Avalonia app

Every other user-facing wlRIX application is C#/Avalonia. This one cannot be: the region selector is a fullscreen
**wlr-layer-shell overlay surface**, and Avalonia's Wayland backend implements no layer shell at all — the same
constraint that made `wlrix-desktop` a Rust client. So it is Rust, drawn with `wlrix-ui` (the Motif bevels, the widgets,
the fonts, the palette), the same shape as `wlrix-desktop` and `wlrix-greeter`.

## How it works

1. **Freeze the screen first.** Every monitor is captured through
   `ext-image-capture-source-v1` + `ext-image-copy-capture-v1` before any surface is mapped. That order is not a detail:
   the overlay covers the desktop, so capturing afterwards would photograph the dialog asking about the picture.
2. **Put an overlay on each monitor**, showing the part of the frozen desktop that falls on it, dimmed outside the
   selection.
3. **Answer**, and save or copy.

The selection lives in **global desktop coordinates** and never learns there is more than one monitor; each overlay
knows only where its own corner sits in that plane. That is what lets a band be dragged across the gap between two
screens.

### Every mode captures whole outputs, including "the active window"

`ext-image-capture-source-v1` has a per-toplevel source and it is deliberately unused. The compositor draws a window
capture from the client's own surface tree, and wlRIX's 4Dwm frame is drawn by the compositor *outside* that tree — so a
window shot that way comes back with no titlebar.

Instead the compositor, which knows which window is focused and owns `decoration::frame_rect`, hands the rectangle over
on the command line: `wlrix-screenshot --select X,Y,W,H`. One capture path, and the frame is right by construction.

## In the overlay

|                            |                                                    |
|----------------------------|----------------------------------------------------|
| drag                       | pull out a selection                               |
| drag a handle              | resize from the opposite corner or edge            |
| drag the middle            | move the whole selection                           |
| arrows                     | nudge one pixel; **Shift** ten, **Ctrl** to resize |
| `Ctrl+A`                   | select the whole desktop                           |
| `Enter` / `Ctrl+C` / `Esc` | save / copy / cancel                               |

Dragging a handle past the opposite edge flips the selection rather than collapsing it, and a selection dragged against
the screen edge **slides** rather than being clipped — clipping it would shrink it, and once shrunk there is no way to
get the size back except by starting over.

## Command line

```
wlrix-screenshot                  # drag a region out (the default)
wlrix-screenshot --all            # start with the whole desktop selected
wlrix-screenshot --select X,Y,W,H # start with that rectangle selected
wlrix-screenshot --output PATH    # where a save goes
wlrix-screenshot --copy           # no overlay: capture, copy, exit
wlrix-screenshot --no-ui          # no overlay: capture, save, exit
```

Exit codes are the ones `xdg-desktop-portal-wlrix` already expects from a helper: **0** taken, **1** canceled, **2**
bad arguments, **3** failed. Both the code and the output are checked by callers, so a run that dies mid-answer cannot
be mistaken for a screenshot that happened.

## Configuration

`$XDG_CONFIG_HOME/wlrix/screenshot.toml`, else `/etc/wlrix/screenshot.toml`. There is no file by default. Unknown keys
are an error, as elsewhere in wlRIX.

```toml
[save]
dir = "~/Pictures/Screenshots"              # default: the XDG pictures dir + /Screenshots
filename = "Screenshot_%Y-%m-%d_%H-%M-%S"   # strftime, without the extension

[capture]
cursor = false        # draw the pointer into the shot

[appearance]
palette = "gotham"    # the color scheme; default is "classic"
dim = 0.55            # how far the unselected area is darkened, 0.0 to 1.0
```

`filename` goes to the C library's `strftime`, so every conversion `strftime(3)` documents works. The **expansion** is
sanitized, not the template: `%x` is a locale's date and several locales put slashes in it, which would silently turn
one name into a path.

## The clipboard, and why a second process appears

Wayland's clipboard is pull-based. Setting a selection hands the compositor no data; it registers *this client* as the
thing to ask, and the bytes are only sent when somebody pastes. A program that sets a selection and exits has therefore
set nothing.

So `--copy` starts a second copy of this program — through `/proc/self/exe`, with the PNG on its stdin — which holds the
selection and answers for it until something else takes the clipboard. `wl-copy` forks for the same reason; a child
process is used here rather than
`fork()` because forking a process that has already loaded fonts and opened a compositor connection is only defined for
async-signal-safe work, which neither of those is.

It speaks `zwlr_data_control_manager_v1` rather than `wl_data_device`, because the latter needs a serial from a recent
input event on a seat the client has focus on — and a background clipboard owner has no window at all.

## The portal

`xdg-desktop-portal-wlrix` implements `org.freedesktop.impl.portal.Screenshot` by spawning this program with `--portal`:
a JSON manifest on stdin, a JSON answer on stdout. See that repo's README for the contract, and `src/portal.rs` here for
this side of it.

The portal names the output file rather than letting this program choose — a portal screenshot is not one the user asked
to keep, so it goes to `$XDG_RUNTIME_DIR` and not to Pictures.

## Testing

The overlay needs a compositor with `wlr-layer-shell` and the two `ext-image-*` protocols, which today means wlRIX's
own. The nested (winit) compositor works and needs no TTY:

```sh
just nested          # from wlrix-epoch: an isolated nested compositor
```

> **The nested compositor's own window has to be *visible* on the host session.** It renders
> when the host schedules it a frame, and a host that has stopped presenting it — the window
> occluded, on another desk, or the screen blanked — means no frames, which means every capture
> times out after two seconds with `did not answer in time`. That is the rig, not the code, and
> it is the same trap as "the traffic-generating window is not optional" one layer further out.

```sh
# Non-interactive, and check it is not a black rectangle.
WAYLAND_DISPLAY=wayland-1 wlrix-screenshot --all --no-ui --output /tmp/shot.png

# The clipboard round trip, without wl-clipboard installed.
WAYLAND_DISPLAY=wayland-1 wlrix-screenshot --select 0,0,400,120 --copy
cargo run --example paste_probe -- /tmp/paste.png
```

`cargo test` covers the parts that need no compositor, which is most of the interesting ones:
the selection model (handles, flipping, sliding, clamping), the cropping across two monitors, the PNG encoding, the
filename expansion, and the `user-dirs.dirs` parser.

## Not built yet

- **Annotation tools.** Spectacle's pen, arrow, rectangle, highlighter and pixelate. The overlay is laid out for a
  toolbar; there is none.
- **Save As…** There is no file dialog in wlRIX yet, so there is nothing honest to open. `Save`
  goes to the configured directory and `--output` names a path.
- **PickColor**, which the portal advertises through this program and answers as unimplemented. The overlay already
  holds the frozen pixels, so it is a crosshair and a loupe away.
- **HiDPI.** Everything is drawn at scale 1, as `wlrix-desktop` does. A monitor whose logical size differs from its
  pixel size is reported on stderr rather than quietly mis-selected — a screenshot is exactly where fractional scale
  would show.
