#!/usr/bin/env just --justfile
name := 'wlrix-screenshot'

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))
bin-dir := base-dir / 'bin'
desktop-dir := base-dir / 'share' / 'applications'

bin-src := 'target' / 'release' / name
bin-dst := bin-dir / name

# The Toolchest builds its menu by scanning desktop entries, so this is what puts the tool in
# it. Not a file handler -- there is no MimeType to register and so no
# `update-desktop-database` to run.
desktop-src := 'data' / 'com.wlrix.screenshot.desktop'
desktop-dst := desktop-dir / 'com.wlrix.screenshot.desktop' 

default:
  @just --list

release:
  cargo build --release

lint:
  cargo clippy

test:
  cargo test

# Install the screenshot tool.
#
# `bin`, not `lib`: this is started *by name* off PATH -- by the compositor from a keybind, by
# `xdg-desktop-portal-wlrix` for the Screenshot interface, and by a person from a terminal --
# the same as every other wlRIX component except the portal backend, which is bus-activated.
#
# Deliberately does not build: this is normally run as root, and building as root leaves a
# target directory nobody can write to afterwards.
#
#     just release && sudo just install
[doc("Install the screenshot tool (build first; run as root)")]
install:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ ! -x '{{bin-src}}' ]; then
    echo "no release build -- run 'just release' first" >&2
    exit 1
  fi
  install -Dm0755 '{{bin-src}}' '{{bin-dst}}'
  install -Dm0644 '{{desktop-src}}' '{{desktop-dst}}'
  echo "installed {{bin-dst}}"
  echo "installed {{desktop-dst}}"
  echo
  echo "wlrix-compositor binds Print, Alt+Print and Shift+Print to this, and"
  echo "xdg-desktop-portal-wlrix spawns it for org.freedesktop.impl.portal.Screenshot."
  echo "Both find it by name on PATH, so neither needs configuring."

[doc("Remove what install put down")]
uninstall:
  #!/usr/bin/env bash
  set -euo pipefail
  rm -f '{{bin-dst}}' '{{desktop-dst}}'
  echo "removed {{bin-dst}} and {{desktop-dst}}"

clean:
  cargo clean
