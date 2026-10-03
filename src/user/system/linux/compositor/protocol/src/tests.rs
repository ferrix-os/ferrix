//! The generated tables against libwayland's own.
//!
//! `tools/common/gen/gen-wayland-protocol.py` reads the same XML `wayland-scanner`
//! reads and could read it wrong -- an opcode off by one, an argument type
//! confused, a nullable flag dropped -- and nothing in the generator would
//! notice. `probe/interfaces.c` links against the real `wl_*_interface`
//! structures and prints what they say; `probe/interfaces.txt` is that
//! output, committed. Everything below compares the two.

use std::collections::BTreeMap;

use compositor_wire::{ArgType, Interface};

use crate::{
    GLOBALS, alpha_modifier, background_effect, capture_source, commit_timing, content_type, core,
    cursor_shape, data_control, ext_data_control, ext_workspace, fifo, focus_grab, foreign_list,
    foreign_toplevel, fractional_scale, gamma_control, global_shortcuts, hotkey, hyprland_surface,
    idle_inhibit, idle_notify, image_copy, input_method, kde_decoration, layer_shell, linux_dmabuf,
    lock_notify, output_management, output_power, pointer_constraints, pointer_gestures,
    pointer_warp, presentation, primary_selection, relative_pointer, screencopy, security_context,
    session_lock, shortcuts_inhibit, single_pixel, system_bell, tearing_control, text_input,
    toplevel_export, toplevel_icon, toplevel_mapping, toplevel_tag, viewporter, virtual_keyboard,
    virtual_pointer, xdg_activation, xdg_decoration, xdg_dialog, xdg_output, xdg_shell,
};

/// The probe's output: one line per message, and one more per unnamed
/// `new_id`.
const PROBE: &str = include_str!("../probe/interfaces.txt");

/// Every interface the crate has a table for, by protocol name.
fn tables() -> Vec<&'static Interface> {
    vec![
        &core::WL_DISPLAY,
        &core::WL_REGISTRY,
        &core::WL_CALLBACK,
        &core::WL_COMPOSITOR,
        &core::WL_SHM_POOL,
        &core::WL_SHM,
        &core::WL_BUFFER,
        &core::WL_DATA_OFFER,
        &core::WL_DATA_SOURCE,
        &core::WL_DATA_DEVICE,
        &core::WL_DATA_DEVICE_MANAGER,
        &core::WL_SHELL,
        &core::WL_SHELL_SURFACE,
        &core::WL_SURFACE,
        &core::WL_SEAT,
        &core::WL_POINTER,
        &core::WL_KEYBOARD,
        &core::WL_TOUCH,
        &core::WL_OUTPUT,
        &core::WL_REGION,
        &core::WL_SUBCOMPOSITOR,
        &core::WL_SUBSURFACE,
        &xdg_shell::XDG_WM_BASE,
        &xdg_shell::XDG_POSITIONER,
        &xdg_shell::XDG_SURFACE,
        &xdg_shell::XDG_TOPLEVEL,
        &xdg_shell::XDG_POPUP,
        &xdg_decoration::ZXDG_DECORATION_MANAGER_V1,
        &xdg_decoration::ZXDG_TOPLEVEL_DECORATION_V1,
        &layer_shell::ZWLR_LAYER_SHELL_V1,
        &layer_shell::ZWLR_LAYER_SURFACE_V1,
        &foreign_toplevel::ZWLR_FOREIGN_TOPLEVEL_MANAGER_V1,
        &foreign_toplevel::ZWLR_FOREIGN_TOPLEVEL_HANDLE_V1,
        &screencopy::ZWLR_SCREENCOPY_MANAGER_V1,
        &screencopy::ZWLR_SCREENCOPY_FRAME_V1,
        &session_lock::EXT_SESSION_LOCK_MANAGER_V1,
        &session_lock::EXT_SESSION_LOCK_V1,
        &session_lock::EXT_SESSION_LOCK_SURFACE_V1,
        &cursor_shape::WP_CURSOR_SHAPE_MANAGER_V1,
        &cursor_shape::WP_CURSOR_SHAPE_DEVICE_V1,
        &primary_selection::ZWP_PRIMARY_SELECTION_DEVICE_MANAGER_V1,
        &primary_selection::ZWP_PRIMARY_SELECTION_DEVICE_V1,
        &primary_selection::ZWP_PRIMARY_SELECTION_OFFER_V1,
        &primary_selection::ZWP_PRIMARY_SELECTION_SOURCE_V1,
        &xdg_activation::XDG_ACTIVATION_V1,
        &xdg_activation::XDG_ACTIVATION_TOKEN_V1,
        &viewporter::WP_VIEWPORTER,
        &viewporter::WP_VIEWPORT,
        &fractional_scale::WP_FRACTIONAL_SCALE_MANAGER_V1,
        &fractional_scale::WP_FRACTIONAL_SCALE_V1,
        &toplevel_icon::XDG_TOPLEVEL_ICON_MANAGER_V1,
        &toplevel_icon::XDG_TOPLEVEL_ICON_V1,
        &text_input::ZWP_TEXT_INPUT_MANAGER_V3,
        &text_input::ZWP_TEXT_INPUT_V3,
        &input_method::ZWP_INPUT_METHOD_MANAGER_V2,
        &input_method::ZWP_INPUT_METHOD_V2,
        &input_method::ZWP_INPUT_POPUP_SURFACE_V2,
        &input_method::ZWP_INPUT_METHOD_KEYBOARD_GRAB_V2,
        &xdg_output::ZXDG_OUTPUT_MANAGER_V1,
        &xdg_output::ZXDG_OUTPUT_V1,
        &presentation::WP_PRESENTATION,
        &presentation::WP_PRESENTATION_FEEDBACK,
        &idle_notify::EXT_IDLE_NOTIFIER_V1,
        &idle_notify::EXT_IDLE_NOTIFICATION_V1,
        &idle_inhibit::ZWP_IDLE_INHIBIT_MANAGER_V1,
        &idle_inhibit::ZWP_IDLE_INHIBITOR_V1,
        &single_pixel::WP_SINGLE_PIXEL_BUFFER_MANAGER_V1,
        &content_type::WP_CONTENT_TYPE_MANAGER_V1,
        &content_type::WP_CONTENT_TYPE_V1,
        &alpha_modifier::WP_ALPHA_MODIFIER_V1,
        &alpha_modifier::WP_ALPHA_MODIFIER_SURFACE_V1,
        &xdg_dialog::XDG_WM_DIALOG_V1,
        &xdg_dialog::XDG_DIALOG_V1,
        &system_bell::XDG_SYSTEM_BELL_V1,
        &toplevel_tag::XDG_TOPLEVEL_TAG_MANAGER_V1,
        &kde_decoration::ORG_KDE_KWIN_SERVER_DECORATION_MANAGER,
        &kde_decoration::ORG_KDE_KWIN_SERVER_DECORATION,
        &relative_pointer::ZWP_RELATIVE_POINTER_MANAGER_V1,
        &relative_pointer::ZWP_RELATIVE_POINTER_V1,
        &pointer_constraints::ZWP_POINTER_CONSTRAINTS_V1,
        &pointer_constraints::ZWP_LOCKED_POINTER_V1,
        &pointer_constraints::ZWP_CONFINED_POINTER_V1,
        &pointer_gestures::ZWP_POINTER_GESTURES_V1,
        &pointer_gestures::ZWP_POINTER_GESTURE_SWIPE_V1,
        &pointer_gestures::ZWP_POINTER_GESTURE_PINCH_V1,
        &pointer_gestures::ZWP_POINTER_GESTURE_HOLD_V1,
        &shortcuts_inhibit::ZWP_KEYBOARD_SHORTCUTS_INHIBIT_MANAGER_V1,
        &shortcuts_inhibit::ZWP_KEYBOARD_SHORTCUTS_INHIBITOR_V1,
        &virtual_keyboard::ZWP_VIRTUAL_KEYBOARD_MANAGER_V1,
        &virtual_keyboard::ZWP_VIRTUAL_KEYBOARD_V1,
        &virtual_pointer::ZWLR_VIRTUAL_POINTER_MANAGER_V1,
        &virtual_pointer::ZWLR_VIRTUAL_POINTER_V1,
        &foreign_list::EXT_FOREIGN_TOPLEVEL_LIST_V1,
        &foreign_list::EXT_FOREIGN_TOPLEVEL_HANDLE_V1,
        &gamma_control::ZWLR_GAMMA_CONTROL_MANAGER_V1,
        &gamma_control::ZWLR_GAMMA_CONTROL_V1,
        &output_power::ZWLR_OUTPUT_POWER_MANAGER_V1,
        &output_power::ZWLR_OUTPUT_POWER_V1,
        &data_control::ZWLR_DATA_CONTROL_MANAGER_V1,
        &data_control::ZWLR_DATA_CONTROL_DEVICE_V1,
        &data_control::ZWLR_DATA_CONTROL_SOURCE_V1,
        &data_control::ZWLR_DATA_CONTROL_OFFER_V1,
        &ext_data_control::EXT_DATA_CONTROL_MANAGER_V1,
        &ext_data_control::EXT_DATA_CONTROL_DEVICE_V1,
        &ext_data_control::EXT_DATA_CONTROL_SOURCE_V1,
        &ext_data_control::EXT_DATA_CONTROL_OFFER_V1,
        &output_management::ZWLR_OUTPUT_MANAGER_V1,
        &output_management::ZWLR_OUTPUT_HEAD_V1,
        &output_management::ZWLR_OUTPUT_MODE_V1,
        &output_management::ZWLR_OUTPUT_CONFIGURATION_V1,
        &output_management::ZWLR_OUTPUT_CONFIGURATION_HEAD_V1,
        &ext_workspace::EXT_WORKSPACE_MANAGER_V1,
        &ext_workspace::EXT_WORKSPACE_GROUP_HANDLE_V1,
        &ext_workspace::EXT_WORKSPACE_HANDLE_V1,
        &global_shortcuts::HYPRLAND_GLOBAL_SHORTCUTS_MANAGER_V1,
        &global_shortcuts::HYPRLAND_GLOBAL_SHORTCUT_V1,
        &focus_grab::HYPRLAND_FOCUS_GRAB_MANAGER_V1,
        &focus_grab::HYPRLAND_FOCUS_GRAB_V1,
        &lock_notify::HYPRLAND_LOCK_NOTIFIER_V1,
        &lock_notify::HYPRLAND_LOCK_NOTIFICATION_V1,
        &toplevel_mapping::HYPRLAND_TOPLEVEL_MAPPING_MANAGER_V1,
        &toplevel_mapping::HYPRLAND_TOPLEVEL_WINDOW_MAPPING_HANDLE_V1,
        &hyprland_surface::HYPRLAND_SURFACE_MANAGER_V1,
        &hyprland_surface::HYPRLAND_SURFACE_V1,
        &toplevel_export::HYPRLAND_TOPLEVEL_EXPORT_MANAGER_V1,
        &toplevel_export::HYPRLAND_TOPLEVEL_EXPORT_FRAME_V1,
        &pointer_warp::WP_POINTER_WARP_V1,
        &background_effect::EXT_BACKGROUND_EFFECT_MANAGER_V1,
        &background_effect::EXT_BACKGROUND_EFFECT_SURFACE_V1,
        &tearing_control::WP_TEARING_CONTROL_MANAGER_V1,
        &tearing_control::WP_TEARING_CONTROL_V1,
        &fifo::WP_FIFO_MANAGER_V1,
        &fifo::WP_FIFO_V1,
        &commit_timing::WP_COMMIT_TIMING_MANAGER_V1,
        &commit_timing::WP_COMMIT_TIMER_V1,
        &security_context::WP_SECURITY_CONTEXT_MANAGER_V1,
        &security_context::WP_SECURITY_CONTEXT_V1,
        &hotkey::VICINAE_HOTKEY_MANAGER_V1,
        &hotkey::VICINAE_HOTKEY_V1,
        &capture_source::EXT_IMAGE_CAPTURE_SOURCE_V1,
        &capture_source::EXT_OUTPUT_IMAGE_CAPTURE_SOURCE_MANAGER_V1,
        &capture_source::EXT_FOREIGN_TOPLEVEL_IMAGE_CAPTURE_SOURCE_MANAGER_V1,
        &image_copy::EXT_IMAGE_COPY_CAPTURE_MANAGER_V1,
        &image_copy::EXT_IMAGE_COPY_CAPTURE_SESSION_V1,
        &image_copy::EXT_IMAGE_COPY_CAPTURE_FRAME_V1,
        &image_copy::EXT_IMAGE_COPY_CAPTURE_CURSOR_SESSION_V1,
        &linux_dmabuf::ZWP_LINUX_DMABUF_V1,
        &linux_dmabuf::ZWP_LINUX_BUFFER_PARAMS_V1,
        &linux_dmabuf::ZWP_LINUX_DMABUF_FEEDBACK_V1,
    ]
}

/// One message as libwayland describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Probed {
    version: u32,
    name: String,
    /// Argument types from libwayland's signature letters.
    signature: Vec<ArgType>,
    /// The version the message appeared in, from the signature's digits.
    since: u32,
}

/// libwayland's signature string as argument types.
///
/// Leading digits are the `since` version; a `?` makes the argument after it
/// nullable; every other letter is one argument. `anywhere` names the slots
/// the probe found to be unnamed `new_id`s.
///
/// The one place the two spellings differ is that unnamed `new_id`. The
/// protocol writes it as a single `<arg type="new_id"/>` with no `interface`;
/// libwayland expands it into the three things it is on the wire, so
/// `wl_registry.bind` has the signature `usun` -- a name, an interface, a
/// version and an id. `compositor_wire` keeps the protocol's spelling, one
/// [`ArgType::AnyNewId`] carrying all three, because a server that read them
/// as three separate arguments could not tell this `new_id` from an ordinary
/// one. So the `s` and `u` in front of an unnamed `n` are folded back into
/// it here. The bytes are the same either way, and `src/user/system/linux/compositor/wire`'s own
/// probe is what shows that.
fn parse_signature(text: &str, anywhere: &[usize]) -> (u32, Vec<ArgType>) {
    let mut since = String::new();
    let mut args = Vec::new();
    let mut nullable = false;
    let mut slot = 0;
    for letter in text.chars() {
        if letter.is_ascii_digit() && args.is_empty() && !nullable {
            since.push(letter);
            continue;
        }
        if letter == '?' {
            nullable = true;
            continue;
        }
        let kind = match letter {
            'i' => ArgType::Int,
            'u' => ArgType::Uint,
            'f' => ArgType::Fixed,
            'a' => ArgType::Array,
            'h' => ArgType::Fd,
            's' => ArgType::Str { nullable },
            'o' => ArgType::Object { nullable },
            'n' if anywhere.contains(&slot) => {
                // The interface string and the version in front of it are
                // part of this one argument.
                assert_eq!(
                    args.pop(),
                    Some(ArgType::Uint),
                    "an unnamed new_id without its version in {text:?}"
                );
                assert_eq!(
                    args.pop(),
                    Some(ArgType::Str { nullable: false }),
                    "an unnamed new_id without its interface in {text:?}"
                );
                ArgType::AnyNewId
            }
            'n' => ArgType::NewId,
            other => panic!("unknown signature letter {other:?} in {text:?}"),
        };
        args.push(kind);
        nullable = false;
        slot += 1;
    }
    (since.parse().unwrap_or(1), args)
}

/// One interface's messages, keyed by `("request" | "event", opcode)`.
type Messages = BTreeMap<(String, u16), Probed>;

/// Everything the probe said: each interface's version and its messages.
type Probe = BTreeMap<String, (u32, Messages)>;

/// Everything the probe said, keyed by interface and then by
/// `("request" | "event", opcode)`.
fn probed() -> Probe {
    // The unnamed new_id slots, gathered first: a message's line comes before
    // its `any-new-id` lines, so two passes.
    let mut anywhere: BTreeMap<(String, String, u16), Vec<usize>> = BTreeMap::new();
    for line in PROBE.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.get(5) == Some(&"any-new-id")
            && let (Some(interface), Some(kind), Some(opcode), Some(slot)) =
                (fields.first(), fields.get(2), fields.get(3), fields.get(6))
        {
            let key = (
                (*interface).to_owned(),
                (*kind).to_owned(),
                opcode.parse().expect("an opcode"),
            );
            anywhere
                .entry(key)
                .or_default()
                .push(slot.parse().expect("a slot"));
        }
    }

    let mut out: Probe = BTreeMap::new();
    for line in PROBE.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<&str> = line.split(' ').collect();
        let (Some(interface), Some(version)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        let version: u32 = version.parse().expect("a version");
        let entry = out
            .entry((*interface).to_owned())
            .or_insert((version, BTreeMap::new()));
        entry.0 = version;
        let Some(kind) = fields.get(2) else { continue };
        if *kind == "empty" {
            continue;
        }
        if fields.get(5) == Some(&"any-new-id") {
            continue;
        }
        let (Some(opcode), Some(name)) = (fields.get(3), fields.get(4)) else {
            continue;
        };
        let opcode: u16 = opcode.parse().expect("an opcode");
        // A signature may be empty, in which case the line ends after the
        // name and `split(' ')` gives an empty last field or none at all.
        let text = fields.get(5).copied().unwrap_or("");
        let slots = anywhere
            .get(&((*interface).to_owned(), (*kind).to_owned(), opcode))
            .cloned()
            .unwrap_or_default();
        let (since, signature) = parse_signature(text, &slots);
        let previous = entry.1.insert(
            ((*kind).to_owned(), opcode),
            Probed {
                version,
                name: (*name).to_owned(),
                signature,
                since,
            },
        );
        assert!(previous.is_none(), "{interface} {kind} {opcode} twice");
    }
    out
}

#[test]
fn the_probe_names_the_libwayland_it_came_from() {
    let first = PROBE.lines().next().expect("a first line");
    assert!(
        first.starts_with("# libwayland "),
        "probe/interfaces.txt should say which libwayland wrote it: {first}"
    );
}

#[test]
fn every_generated_table_is_the_one_libwayland_compiled() {
    let probe = probed();
    let mut checked = 0;
    for interface in tables() {
        let (version, messages) = probe
            .get(interface.name)
            .unwrap_or_else(|| panic!("{} is not in probe/interfaces.txt", interface.name));
        assert_eq!(
            interface.version, *version,
            "{}: a different version",
            interface.name
        );
        for (kind, table) in [("request", interface.requests), ("event", interface.events)] {
            for (opcode, method) in table.iter().enumerate() {
                let opcode = u16::try_from(opcode).expect("an opcode fits");
                let want = messages.get(&(kind.to_owned(), opcode)).unwrap_or_else(|| {
                    panic!("{} has no {kind} {opcode} in libwayland", interface.name)
                });
                assert_eq!(
                    method.name, want.name,
                    "{} {kind} {opcode}: a different name",
                    interface.name
                );
                assert_eq!(
                    method.signature, want.signature,
                    "{}.{} ({kind} {opcode}): a different signature",
                    interface.name, method.name
                );
                assert_eq!(
                    method.since, want.since,
                    "{}.{}: a different `since`",
                    interface.name, method.name
                );
                checked += 1;
            }
            // And nothing libwayland has that the table does not.
            let theirs = messages.keys().filter(|(k, _)| k == kind).count();
            assert_eq!(
                table.len(),
                theirs,
                "{} has {} {kind}s and libwayland has {theirs}",
                interface.name,
                table.len()
            );
        }
    }
    assert!(checked > 150, "only {checked} messages were compared");
}

#[test]
fn the_opcode_constants_are_the_tables_own_order() {
    // Every generated `request`/`event` module is a set of constants that
    // must agree with the table beside it, since the server dispatches on
    // one and reads the signature from the other.
    assert_eq!(core::wl_display::request::SYNC, 0);
    assert_eq!(core::wl_display::request::GET_REGISTRY, 1);
    assert_eq!(core::wl_display::event::ERROR, 0);
    assert_eq!(core::wl_display::event::DELETE_ID, 1);
    assert_eq!(core::wl_registry::request::BIND, 0);
    assert_eq!(core::wl_registry::event::GLOBAL, 0);
    assert_eq!(core::wl_surface::request::ATTACH, 1);
    assert_eq!(core::wl_surface::request::COMMIT, 6);
    assert_eq!(xdg_shell::xdg_toplevel::request::SET_TITLE, 2);
    assert_eq!(xdg_shell::xdg_surface::event::CONFIGURE, 0);

    for interface in tables() {
        for (kind, table) in [("request", interface.requests), ("event", interface.events)] {
            for (opcode, method) in table.iter().enumerate() {
                let found = if kind == "request" {
                    interface.request_opcode(method.name)
                } else {
                    interface.event_opcode(method.name)
                };
                assert_eq!(
                    found,
                    u16::try_from(opcode).ok(),
                    "{}.{}",
                    interface.name,
                    method.name
                );
            }
        }
    }
}

#[test]
fn destructors_are_the_ones_the_protocol_marks() {
    // The server has to drop the object when one of these is handled, so a
    // missed `type="destructor"` leaks an id for the life of the connection.
    assert!(destructor(&core::WL_SURFACE, "destroy"));
    assert!(destructor(&core::WL_BUFFER, "destroy"));
    assert!(destructor(&core::WL_SHM_POOL, "destroy"));
    assert!(destructor(&xdg_shell::XDG_TOPLEVEL, "destroy"));
    assert!(destructor(&xdg_shell::XDG_SURFACE, "destroy"));
    assert!(destructor(&layer_shell::ZWLR_LAYER_SURFACE_V1, "destroy"));
    // A release is a destructor too, and `wl_seat.release` is one.
    assert!(destructor(&core::WL_POINTER, "release"));
    assert!(destructor(&core::WL_KEYBOARD, "release"));
    // And these are not.
    assert!(!destructor(&core::WL_SURFACE, "commit"));
    assert!(!destructor(&core::WL_COMPOSITOR, "create_surface"));
    assert!(!destructor(&core::WL_DISPLAY, "sync"));

    // wl_display and wl_registry have no destructor at all: a client cannot
    // destroy either.
    for interface in [&core::WL_DISPLAY, &core::WL_REGISTRY] {
        assert!(
            !interface.requests.iter().any(|method| method.destructor),
            "{} has a destructor",
            interface.name
        );
    }
}

fn destructor(interface: &Interface, name: &str) -> bool {
    interface
        .requests
        .iter()
        .find(|method| method.name == name)
        .is_some_and(|method| method.destructor)
}

#[test]
fn every_global_is_bindable_and_none_is_the_display_or_the_registry() {
    assert!(!GLOBALS.is_empty());
    for global in GLOBALS {
        assert_ne!(global.name, "wl_display");
        assert_ne!(global.name, "wl_registry");
        assert!(global.version >= 1, "{}", global.name);
        // A global a client binds has to be in the table the tests check,
        // or nothing knows its signatures.
        assert!(
            tables().iter().any(|table| table.name == global.name),
            "{} is offered but has no table",
            global.name
        );
    }
    let names: Vec<&str> = GLOBALS.iter().map(|global| global.name).collect();
    assert!(names.contains(&"wl_compositor"));
    assert!(names.contains(&"wl_shm"));
    assert!(names.contains(&"xdg_wm_base"));
    assert!(names.contains(&"zwlr_layer_shell_v1"));
    assert!(names.contains(&"zwlr_foreign_toplevel_manager_v1"));
    assert!(names.contains(&"zwlr_screencopy_manager_v1"));
    assert!(names.contains(&"ext_session_lock_manager_v1"));
    assert!(names.contains(&"wp_cursor_shape_manager_v1"));
    assert!(names.contains(&"zwp_primary_selection_device_manager_v1"));
    assert!(names.contains(&"zwp_text_input_manager_v3"));
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "a global is offered twice");
}

#[test]
fn the_enumerations_carry_the_values_the_protocol_gives() {
    // The ones the server acts on, spot-checked against wayland.xml and
    // xdg-shell.xml so a generator that renumbered an enum is caught.
    assert_eq!(core::wl_display::error::INVALID_OBJECT, 0);
    assert_eq!(core::wl_display::error::INVALID_METHOD, 1);
    assert_eq!(core::wl_display::error::NO_MEMORY, 2);
    assert_eq!(core::wl_display::error::IMPLEMENTATION, 3);
    // wl_shm's two mandatory formats, which are DRM's fourcc codes' aliases.
    assert_eq!(core::wl_shm::format::ARGB8888, 0);
    assert_eq!(core::wl_shm::format::XRGB8888, 1);
    // A bitfield: wl_seat's capabilities.
    assert_eq!(core::wl_seat::capability::POINTER, 1);
    assert_eq!(core::wl_seat::capability::KEYBOARD, 2);
    assert_eq!(core::wl_seat::capability::TOUCH, 4);
    // Key and button state.
    assert_eq!(core::wl_keyboard::key_state::RELEASED, 0);
    assert_eq!(core::wl_keyboard::key_state::PRESSED, 1);
    assert_eq!(core::wl_pointer::button_state::RELEASED, 0);
    assert_eq!(core::wl_pointer::button_state::PRESSED, 1);
    // The keymap format xkbcommon produces.
    assert_eq!(core::wl_keyboard::keymap_format::NO_KEYMAP, 0);
    assert_eq!(core::wl_keyboard::keymap_format::XKB_V1, 1);
    // xdg_toplevel's states, which a tiling compositor sends on every
    // configure.
    assert_eq!(xdg_shell::xdg_toplevel::state::MAXIMIZED, 1);
    assert_eq!(xdg_shell::xdg_toplevel::state::FULLSCREEN, 2);
    assert_eq!(xdg_shell::xdg_toplevel::state::ACTIVATED, 4);
    // Decoration: a tiling compositor always answers client-side.
    assert_eq!(
        xdg_decoration::zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE,
        1
    );
    assert_eq!(
        xdg_decoration::zxdg_toplevel_decoration_v1::mode::SERVER_SIDE,
        2
    );
    // The layers a bar or a wallpaper asks for.
    assert_eq!(layer_shell::zwlr_layer_shell_v1::layer::BACKGROUND, 0);
    assert_eq!(layer_shell::zwlr_layer_shell_v1::layer::OVERLAY, 3);
}

#[test]
fn an_enum_entry_that_starts_with_a_digit_is_still_an_identifier() {
    // wl_output's transforms are named 90, 180 and 270, which cannot start a
    // Rust name; the generator puts an N in front.
    assert_eq!(core::wl_output::transform::NORMAL, 0);
    assert_eq!(core::wl_output::transform::N90, 1);
    assert_eq!(core::wl_output::transform::N180, 2);
    assert_eq!(core::wl_output::transform::N270, 3);
}
