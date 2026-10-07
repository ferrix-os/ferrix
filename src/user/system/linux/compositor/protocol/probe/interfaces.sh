#!/usr/bin/env bash
# Print libwayland's own compiled interface tables, for every protocol the
# compositor speaks.
#
# Needs a Linux host with libwayland's development files (libwayland-dev),
# wayland-scanner and gcc. The core protocol's tables come from libwayland
# itself; the rest are compiled here from the XML vendored beside this
# directory, which is the same XML the generator reads -- so a disagreement
# is the generator's and not a version skew.
#
# Writes interfaces.txt beside this script, which the crate's tests read.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
protocols="$here/../protocols"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

scan() {
    local xml="$1" stem="$2"
    wayland-scanner client-header "$xml" "$work/$stem-client-protocol.h"
    wayland-scanner private-code "$xml" "$work/$stem-protocol.c"
}

scan "$protocols/xdg-shell.xml" xdg-shell
scan "$protocols/xdg-decoration-unstable-v1.xml" xdg-decoration
scan "$protocols/wlr-layer-shell-unstable-v1.xml" wlr-layer-shell
scan "$protocols/wlr-foreign-toplevel-management-unstable-v1.xml" wlr-foreign-toplevel-management
scan "$protocols/wlr-screencopy-unstable-v1.xml" wlr-screencopy
scan "$protocols/ext-session-lock-v1.xml" ext-session-lock
# `cursor-shape-v1` names `zwp_tablet_tool_v2` in one request's signature, so
# the tablet protocol is compiled in for the symbol. It is not one of the
# generator's `FILES`: this compositor has no tablet tool, and a table
# nothing offers is a table nothing checks.
scan "/usr/share/wayland-protocols/stable/tablet/tablet-v2.xml" tablet
scan "$protocols/cursor-shape-v1.xml" cursor-shape
scan "$protocols/primary-selection-unstable-v1.xml" primary-selection
scan "$protocols/xdg-activation-v1.xml" xdg-activation
scan "$protocols/viewporter.xml" viewporter
scan "$protocols/fractional-scale-v1.xml" fractional-scale
scan "$protocols/xdg-toplevel-icon-v1.xml" xdg-toplevel-icon
scan "$protocols/text-input-unstable-v3.xml" text-input
scan "$protocols/input-method-unstable-v2.xml" input-method
scan "$protocols/xdg-output-unstable-v1.xml" xdg-output
scan "$protocols/presentation-time.xml" presentation-time
scan "$protocols/ext-idle-notify-v1.xml" ext-idle-notify
scan "$protocols/idle-inhibit-unstable-v1.xml" idle-inhibit
scan "$protocols/single-pixel-buffer-v1.xml" single-pixel-buffer
scan "$protocols/content-type-v1.xml" content-type
scan "$protocols/alpha-modifier-v1.xml" alpha-modifier
scan "$protocols/xdg-dialog-v1.xml" xdg-dialog
scan "$protocols/xdg-system-bell-v1.xml" xdg-system-bell
scan "$protocols/xdg-toplevel-tag-v1.xml" xdg-toplevel-tag
scan "$protocols/kde-server-decoration.xml" kde-server-decoration
scan "$protocols/relative-pointer-unstable-v1.xml" relative-pointer
scan "$protocols/pointer-constraints-unstable-v1.xml" pointer-constraints
scan "$protocols/pointer-gestures-unstable-v1.xml" pointer-gestures
scan "$protocols/keyboard-shortcuts-inhibit-unstable-v1.xml" keyboard-shortcuts-inhibit
scan "$protocols/virtual-keyboard-unstable-v1.xml" virtual-keyboard
scan "$protocols/wlr-virtual-pointer-unstable-v1.xml" wlr-virtual-pointer
scan "$protocols/ext-foreign-toplevel-list-v1.xml" ext-foreign-toplevel-list
scan "$protocols/wlr-gamma-control-unstable-v1.xml" wlr-gamma-control
scan "$protocols/wlr-output-power-management-unstable-v1.xml" wlr-output-power-management
scan "$protocols/wlr-data-control-unstable-v1.xml" wlr-data-control
scan "$protocols/ext-data-control-v1.xml" ext-data-control
scan "$protocols/wlr-output-management-unstable-v1.xml" wlr-output-management
scan "$protocols/ext-workspace-v1.xml" ext-workspace
scan "$protocols/hyprland-global-shortcuts-v1.xml" hyprland-global-shortcuts
scan "$protocols/hyprland-focus-grab-v1.xml" hyprland-focus-grab
scan "$protocols/hyprland-lock-notify-v1.xml" hyprland-lock-notify
scan "$protocols/hyprland-toplevel-mapping-v1.xml" hyprland-toplevel-mapping
scan "$protocols/hyprland-surface-v1.xml" hyprland-surface
scan "$protocols/hyprland-toplevel-export-v1.xml" hyprland-toplevel-export
scan "$protocols/pointer-warp-v1.xml" pointer-warp
scan "$protocols/ext-background-effect-v1.xml" ext-background-effect
scan "$protocols/tearing-control-v1.xml" tearing-control
scan "$protocols/fifo-v1.xml" fifo
scan "$protocols/commit-timing-v1.xml" commit-timing
scan "$protocols/security-context-v1.xml" security-context
scan "$protocols/vicinae-hotkey-v1.xml" vicinae-hotkey
scan "$protocols/ext-image-capture-source-v1.xml" ext-image-capture-source
scan "$protocols/ext-image-copy-capture-v1.xml" ext-image-copy-capture
scan "$protocols/linux-dmabuf-v1.xml" linux-dmabuf

gcc -O0 -Wall -Werror -I"$work" \
    $(pkg-config --cflags wayland-client) \
    -o "$work/interfaces" "$here/interfaces.c" \
    "$work"/*-protocol.c \
    $(pkg-config --libs wayland-client)
"$work/interfaces" > "$here/interfaces.txt"
echo "wrote $here/interfaces.txt ($(wc -l < "$here/interfaces.txt") lines)"
