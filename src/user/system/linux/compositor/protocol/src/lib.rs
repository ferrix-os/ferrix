//! The Wayland protocols the compositor speaks.
//!
//! Every interface here is a table generated from the protocol's own XML by
//! `tools/common/gen/gen-wayland-protocol.py`, which `cargo xtask check` runs with
//! `--check` so a hand edit cannot drift from the file it came from. The XML
//! is vendored under `protocols/`, not read from the machine: a table built
//! from whatever `wayland-protocols` the builder happened to have installed
//! would change under the compositor without a commit.
//!
//! What the tables are *for* is `compositor_wire`: the wire format is
//! untyped, so a reader has to be told the signature of what it is about to
//! read, and [`Interface`] is where that comes from.
//!
//! # What is here
//!
//! * **`core`** is `wayland.xml`: `wl_display`, `wl_registry`,
//!   `wl_compositor` and `wl_surface`, `wl_shm`, `wl_seat` with its
//!   keyboard and pointer, `wl_output`, `wl_subcompositor` and the data
//!   device.
//! * **`xdg_shell`** is how an application gets a window.
//! * **`xdg_decoration`** is who draws the title bar, which for a tiling
//!   compositor is always the client-side answer of "neither".
//! * **`layer_shell`** is `zwlr_layer_shell_v1`, which is how a bar or a
//!   wallpaper places itself. Hyprland's own bars use it.
//! * **`foreign_toplevel`** is `zwlr_foreign_toplevel_management_v1`: the
//!   list of windows, and the four things a bar does with one. It is what
//!   fills a taskbar, and it is the other half of a bar's job -- layer-shell
//!   puts the bar on the screen and this tells it what to draw.
//! * **`screencopy`** is `zwlr_screencopy_v1`: a screenshot. `grim`,
//!   `hyprshot` and every screen recorder on wlroots go through it.
//! * **`cursor_shape`** is `wp_cursor_shape_v1`: a client naming the cursor
//!   it wants -- an I-beam, a hand, a resize arrow -- rather than drawing
//!   one, which is what a toolkit prefers and what a compositor with a
//!   theme is for.
//! * **`primary_selection`** is the middle-click paste, which is the
//!   selection's older and simpler sibling.
//! * **`xdg_activation`** is one program asking the compositor to focus
//!   another: a link opened from a chat window raising the browser.
//! * **`viewporter`** is a client saying its buffer is to be cropped or
//!   scaled into its surface, and **`fractional_scale`** is the compositor
//!   telling it a scale that is not a whole number.
//! * **`toplevel_icon`** is the icon a taskbar draws beside a window's
//!   name.
//! * **`text_input`** and **`input_method`** are the two halves of typing
//!   through an input method: the application says it wants text, the
//!   method says what was typed, and the compositor is what joins them.
//! * **`session_lock`** is `ext-session-lock-v1`: the screen lock.
//!   `hyprlock` and `swaylock` speak it, and it is the one protocol whose
//!   whole point is that the compositor stops drawing everything else.
//!
//! * **`xdg_output`** is a screen's logical position, size and name, which
//!   is what a bar reads rather than `wl_output.mode`: the mode is in the
//!   screen's own pixels and a bar lays itself out in the logical ones.
//! * **`presentation`** is `presentation-time`: when a frame actually
//!   reached the screen, which is what a toolkit that animates needs and
//!   what a frame callback does not say.
//! * **`idle_notify`** and **`idle_inhibit`** are the two halves of "is
//!   anyone there": a screen locker waits on the first and a video player
//!   holds it off with the second.
//! * **`single_pixel`** is one `wl_buffer` of one colour, which is how a
//!   client puts a solid rectangle on the screen without sharing a
//!   megabyte of the same four bytes.
//! * **`content_type`** is a client saying it is showing a video or a
//!   game, and **`alpha_modifier`** is it asking to be drawn see-through.
//! * **`xdg_dialog`** is a dialog saying it is modal, which is what floats
//!   it here; **`system_bell`** is the terminal bell; **`toplevel_tag`** is
//!   a name a window keeps across restarts.
//! * **`kde_decoration`** is KDE's own `xdg-decoration`, which a good deal
//!   of software still asks first.
//!
//! * **`relative_pointer`** and **`pointer_constraints`** are the pair a
//!   game, a 3D modeller or a remote-desktop viewer needs: how far the
//!   pointer *moved*, and keeping it inside the window while they have it.
//! * **`pointer_gestures`** is a touchpad's swipe, pinch and hold.
//! * **`shortcuts_inhibit`** is how a virtual machine or a nested
//!   compositor gets `SUPER` instead of the compositor eating it.
//! * **`virtual_keyboard`** and **`virtual_pointer`** are a client acting
//!   as a device: `wtype`, `ydotool` and every on-screen keyboard.
//!
//! * **`foreign_list`** is `ext-foreign-toplevel-list-v1`: the window list
//!   as the newer specification has it, which is the one a taskbar written
//!   this year binds.
//! * **`gamma_control`** is what `gammastep` and `hyprsunset` use to make
//!   the screen warmer at night, and **`output_power`** is `wlopm` turning
//!   a screen off.
//! * **`data_control`** and **`ext_data_control`** are the clipboard as a
//!   *manager* sees it: `cliphist` and `wl-paste --watch` have no window at
//!   all and must be told every time anything is copied. They are the same
//!   protocol twice, wlroots' and the standardised one.
//! * **`output_management`** is `kanshi` and `wlr-randr` arranging the
//!   screens, and **`ext_workspace`** is the workspace numbers a bar draws.
//!
//! * **`global_shortcuts`**, **`focus_grab`**, **`lock_notify`**,
//!   **`toplevel_mapping`**, **`hyprland_surface`** and
//!   **`toplevel_export`** are Hyprland's own: a shortcut a program
//!   registers rather than a keybind, a launcher holding the focus, a
//!   program told when the screen locks, the handle that joins a
//!   `wl_surface` to a window everywhere else, a surface's own opacity, and
//!   a screenshot of one *window* rather than a screen.
//!
//! * **`pointer_warp`** is a client putting the pointer somewhere inside
//!   its own window, which a game's settings panel and a drawing program
//!   both want; **`background_effect`** is a surface asking for what is
//!   behind it to be blurred, which is `layerrule = blur` said by the
//!   protocol.
//! * **`tearing_control`**, **`fifo`** and **`commit_timing`** are a
//!   client saying how it would like its frames scheduled.
//! * **`security_context`** is a sandbox asking for a socket of its own,
//!   so the compositor can tell a flatpak's clients from the rest.
//! * **`hotkey`** is `vicinae`'s, a launcher that asks for a key by keysym
//!   rather than registering a name.
//!
//! * **`capture_source`** and **`image_copy`** are screenshots as the
//!   `ext` namespace has them: a *source* -- a screen, or a window from
//!   either toplevel list -- and a session that copies frames out of it.
//!   The newer `grim` and every screen-sharing portal written this year
//!   bind these rather than `zwlr_screencopy_v1`.
//! * **`linux_dmabuf`** is `zwp_linux_dmabuf_v1`: a buffer that lives on
//!   the GPU, handed over as a descriptor rather than as shared memory, so a
//!   client that draws on the GPU is shown without its pixels coming back
//!   (`docs/GPU.md` §3.13). Offered only where the compositor draws on the
//!   GPU, so it is not in [`GLOBALS`].
//!
//! The list of protocols is `FILES` in the generator. Adding one is vendoring
//! its XML, adding a line there and a module to `generated/mod.rs`, and
//! naming its interfaces in `probe/interfaces.c` and `probe/interfaces.sh`,
//! so that the new tables are compared against libwayland's compiled ones as
//! every other table here is.

mod generated;

pub use compositor_wire::Interface;
pub use generated::{
    alpha_modifier, background_effect, capture_source, commit_timing, content_type, core,
    cursor_shape, data_control, ext_data_control, ext_workspace, fifo, focus_grab, foreign_list,
    foreign_toplevel, fractional_scale, gamma_control, global_shortcuts, hotkey, hyprland_surface,
    idle_inhibit, idle_notify, image_copy, input_method, kde_decoration, layer_shell, linux_dmabuf,
    lock_notify, output_management, output_power, pointer_constraints, pointer_gestures,
    pointer_warp, presentation, primary_selection, relative_pointer, screencopy, security_context,
    session_lock, shortcuts_inhibit, single_pixel, system_bell, tearing_control, text_input,
    toplevel_export, toplevel_icon, toplevel_mapping, toplevel_tag, viewporter, virtual_keyboard,
    virtual_pointer, xdg_activation, xdg_decoration, xdg_dialog, xdg_output, xdg_shell,
};

/// Every interface the compositor offers as a global, with the version it
/// offers, in the order `wl_registry.global` announces them.
///
/// `wl_display` and `wl_registry` are not here: a connection starts with the
/// first and asks for the second, and neither is ever bound.
pub const GLOBALS: &[&Interface] = &[
    &core::WL_COMPOSITOR,
    &core::WL_SUBCOMPOSITOR,
    &core::WL_SHM,
    &core::WL_SEAT,
    &core::WL_OUTPUT,
    &core::WL_DATA_DEVICE_MANAGER,
    &xdg_shell::XDG_WM_BASE,
    &xdg_decoration::ZXDG_DECORATION_MANAGER_V1,
    &layer_shell::ZWLR_LAYER_SHELL_V1,
    &foreign_toplevel::ZWLR_FOREIGN_TOPLEVEL_MANAGER_V1,
    &screencopy::ZWLR_SCREENCOPY_MANAGER_V1,
    &session_lock::EXT_SESSION_LOCK_MANAGER_V1,
    &cursor_shape::WP_CURSOR_SHAPE_MANAGER_V1,
    &primary_selection::ZWP_PRIMARY_SELECTION_DEVICE_MANAGER_V1,
    &xdg_activation::XDG_ACTIVATION_V1,
    &viewporter::WP_VIEWPORTER,
    &fractional_scale::WP_FRACTIONAL_SCALE_MANAGER_V1,
    &toplevel_icon::XDG_TOPLEVEL_ICON_MANAGER_V1,
    &text_input::ZWP_TEXT_INPUT_MANAGER_V3,
    &input_method::ZWP_INPUT_METHOD_MANAGER_V2,
    &xdg_output::ZXDG_OUTPUT_MANAGER_V1,
    &presentation::WP_PRESENTATION,
    &idle_notify::EXT_IDLE_NOTIFIER_V1,
    &idle_inhibit::ZWP_IDLE_INHIBIT_MANAGER_V1,
    &single_pixel::WP_SINGLE_PIXEL_BUFFER_MANAGER_V1,
    &content_type::WP_CONTENT_TYPE_MANAGER_V1,
    &alpha_modifier::WP_ALPHA_MODIFIER_V1,
    &xdg_dialog::XDG_WM_DIALOG_V1,
    &system_bell::XDG_SYSTEM_BELL_V1,
    &toplevel_tag::XDG_TOPLEVEL_TAG_MANAGER_V1,
    &kde_decoration::ORG_KDE_KWIN_SERVER_DECORATION_MANAGER,
    &relative_pointer::ZWP_RELATIVE_POINTER_MANAGER_V1,
    &pointer_constraints::ZWP_POINTER_CONSTRAINTS_V1,
    &pointer_gestures::ZWP_POINTER_GESTURES_V1,
    &shortcuts_inhibit::ZWP_KEYBOARD_SHORTCUTS_INHIBIT_MANAGER_V1,
    &virtual_keyboard::ZWP_VIRTUAL_KEYBOARD_MANAGER_V1,
    &virtual_pointer::ZWLR_VIRTUAL_POINTER_MANAGER_V1,
    &foreign_list::EXT_FOREIGN_TOPLEVEL_LIST_V1,
    &gamma_control::ZWLR_GAMMA_CONTROL_MANAGER_V1,
    &output_power::ZWLR_OUTPUT_POWER_MANAGER_V1,
    &data_control::ZWLR_DATA_CONTROL_MANAGER_V1,
    &ext_data_control::EXT_DATA_CONTROL_MANAGER_V1,
    &output_management::ZWLR_OUTPUT_MANAGER_V1,
    &ext_workspace::EXT_WORKSPACE_MANAGER_V1,
    &global_shortcuts::HYPRLAND_GLOBAL_SHORTCUTS_MANAGER_V1,
    &focus_grab::HYPRLAND_FOCUS_GRAB_MANAGER_V1,
    &lock_notify::HYPRLAND_LOCK_NOTIFIER_V1,
    &toplevel_mapping::HYPRLAND_TOPLEVEL_MAPPING_MANAGER_V1,
    &hyprland_surface::HYPRLAND_SURFACE_MANAGER_V1,
    &toplevel_export::HYPRLAND_TOPLEVEL_EXPORT_MANAGER_V1,
    &pointer_warp::WP_POINTER_WARP_V1,
    &background_effect::EXT_BACKGROUND_EFFECT_MANAGER_V1,
    &tearing_control::WP_TEARING_CONTROL_MANAGER_V1,
    &fifo::WP_FIFO_MANAGER_V1,
    &commit_timing::WP_COMMIT_TIMING_MANAGER_V1,
    &security_context::WP_SECURITY_CONTEXT_MANAGER_V1,
    &hotkey::VICINAE_HOTKEY_MANAGER_V1,
    &capture_source::EXT_OUTPUT_IMAGE_CAPTURE_SOURCE_MANAGER_V1,
    &capture_source::EXT_FOREIGN_TOPLEVEL_IMAGE_CAPTURE_SOURCE_MANAGER_V1,
    &image_copy::EXT_IMAGE_COPY_CAPTURE_MANAGER_V1,
];

#[cfg(test)]
mod tests;
