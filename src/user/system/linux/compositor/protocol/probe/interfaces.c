/*
 * Print libwayland's own compiled interface tables, so the tables generated
 * from the XML can be required to match them.
 *
 * `tools/common/gen/gen-wayland-protocol.py` reads the same XML `wayland-scanner`
 * reads, and could read it wrong: an opcode off by one, an argument type
 * confused, a nullable flag dropped. Nothing in the generator would notice.
 * This links against the real `wl_*_interface` structures -- libwayland's own
 * for the core protocol, and wayland-scanner's output for the rest -- and
 * prints what they say, in the order they say it.
 *
 * One line per message:
 *
 *     <interface> <version> <request|event> <opcode> <name> <signature>
 *
 * The signature is libwayland's own string, from `struct wl_message`: a
 * `since` version as leading digits, `?` before a nullable argument, and then
 * one letter per argument -- i u f s o n a h, and `n` with no interface in
 * `types` is the unnamed new_id of `wl_registry.bind`. `src/tests.rs` reads
 * the committed output and requires the generated tables to agree.
 */

#include <stdio.h>
#include <string.h>

#include <wayland-client.h>
#include <wayland-client-protocol.h>

#include "xdg-shell-client-protocol.h"
#include "xdg-decoration-client-protocol.h"
#include "wlr-layer-shell-client-protocol.h"
#include "wlr-foreign-toplevel-management-client-protocol.h"
#include "wlr-screencopy-client-protocol.h"
#include "ext-session-lock-client-protocol.h"
#include "cursor-shape-client-protocol.h"
#include "primary-selection-client-protocol.h"
#include "xdg-activation-client-protocol.h"
#include "viewporter-client-protocol.h"
#include "fractional-scale-client-protocol.h"
#include "xdg-toplevel-icon-client-protocol.h"
#include "text-input-client-protocol.h"
#include "input-method-client-protocol.h"
#include "xdg-output-client-protocol.h"
#include "presentation-time-client-protocol.h"
#include "ext-idle-notify-client-protocol.h"
#include "idle-inhibit-client-protocol.h"
#include "single-pixel-buffer-client-protocol.h"
#include "content-type-client-protocol.h"
#include "alpha-modifier-client-protocol.h"
#include "xdg-dialog-client-protocol.h"
#include "xdg-system-bell-client-protocol.h"
#include "xdg-toplevel-tag-client-protocol.h"
#include "kde-server-decoration-client-protocol.h"
#include "relative-pointer-client-protocol.h"
#include "pointer-constraints-client-protocol.h"
#include "pointer-gestures-client-protocol.h"
#include "keyboard-shortcuts-inhibit-client-protocol.h"
#include "virtual-keyboard-client-protocol.h"
#include "wlr-virtual-pointer-client-protocol.h"
#include "ext-foreign-toplevel-list-client-protocol.h"
#include "wlr-gamma-control-client-protocol.h"
#include "wlr-output-power-management-client-protocol.h"
#include "wlr-data-control-client-protocol.h"
#include "ext-data-control-client-protocol.h"
#include "wlr-output-management-client-protocol.h"
#include "ext-workspace-client-protocol.h"
#include "hyprland-global-shortcuts-client-protocol.h"
#include "hyprland-focus-grab-client-protocol.h"
#include "hyprland-lock-notify-client-protocol.h"
#include "hyprland-toplevel-mapping-client-protocol.h"
#include "hyprland-surface-client-protocol.h"
#include "hyprland-toplevel-export-client-protocol.h"
#include "pointer-warp-client-protocol.h"
#include "ext-background-effect-client-protocol.h"
#include "tearing-control-client-protocol.h"
#include "fifo-client-protocol.h"
#include "commit-timing-client-protocol.h"
#include "security-context-client-protocol.h"
#include "vicinae-hotkey-client-protocol.h"
#include "ext-image-capture-source-client-protocol.h"
#include "ext-image-copy-capture-client-protocol.h"
#include "linux-dmabuf-client-protocol.h"

static void print_messages(const struct wl_interface *interface,
			   const char *kind, const struct wl_message *messages,
			   int count)
{
	for (int opcode = 0; opcode < count; opcode++) {
		const struct wl_message *message = &messages[opcode];
		printf("%s %d %s %d %s %s\n", interface->name,
		       interface->version, kind, opcode, message->name,
		       message->signature);
		/* An `n` whose entry in `types` is NULL is the unnamed new_id;
		 * say so, since the signature letter alone does not. */
		int slot = 0;
		for (const char *c = message->signature; *c; c++) {
			if (*c >= '0' && *c <= '9')
				continue;
			if (*c == '?')
				continue;
			if (*c == 'n' && message->types &&
			    message->types[slot] == NULL)
				printf("%s %d %s %d %s any-new-id %d\n",
				       interface->name, interface->version,
				       kind, opcode, message->name, slot);
			slot++;
		}
	}
}

static void print_interface(const struct wl_interface *interface)
{
	print_messages(interface, "request", interface->methods,
		       interface->method_count);
	print_messages(interface, "event", interface->events,
		       interface->event_count);
	if (interface->method_count == 0 && interface->event_count == 0)
		printf("%s %d empty\n", interface->name, interface->version);
}

int main(void)
{
	const struct wl_interface *interfaces[] = {
		/* wayland.xml */
		&wl_display_interface,
		&wl_registry_interface,
		&wl_callback_interface,
		&wl_compositor_interface,
		&wl_shm_pool_interface,
		&wl_shm_interface,
		&wl_buffer_interface,
		&wl_data_offer_interface,
		&wl_data_source_interface,
		&wl_data_device_interface,
		&wl_data_device_manager_interface,
		&wl_shell_interface,
		&wl_shell_surface_interface,
		&wl_surface_interface,
		&wl_seat_interface,
		&wl_pointer_interface,
		&wl_keyboard_interface,
		&wl_touch_interface,
		&wl_output_interface,
		&wl_region_interface,
		&wl_subcompositor_interface,
		&wl_subsurface_interface,
		/* xdg-shell.xml */
		&xdg_wm_base_interface,
		&xdg_positioner_interface,
		&xdg_surface_interface,
		&xdg_toplevel_interface,
		&xdg_popup_interface,
		/* xdg-decoration-unstable-v1.xml */
		&zxdg_decoration_manager_v1_interface,
		&zxdg_toplevel_decoration_v1_interface,
		/* wlr-layer-shell-unstable-v1.xml */
		&zwlr_layer_shell_v1_interface,
		&zwlr_layer_surface_v1_interface,
		/* wlr-foreign-toplevel-management-unstable-v1.xml */
		&zwlr_foreign_toplevel_manager_v1_interface,
		&zwlr_foreign_toplevel_handle_v1_interface,
		/* wlr-screencopy-unstable-v1.xml */
		&zwlr_screencopy_manager_v1_interface,
		&zwlr_screencopy_frame_v1_interface,
		/* ext-session-lock-v1.xml */
		&ext_session_lock_manager_v1_interface,
		&ext_session_lock_v1_interface,
		&ext_session_lock_surface_v1_interface,
		/* cursor-shape-v1.xml */
		&wp_cursor_shape_manager_v1_interface,
		&wp_cursor_shape_device_v1_interface,
		/* primary-selection-unstable-v1.xml */
		&zwp_primary_selection_device_manager_v1_interface,
		&zwp_primary_selection_device_v1_interface,
		&zwp_primary_selection_offer_v1_interface,
		&zwp_primary_selection_source_v1_interface,
		/* xdg-activation-v1.xml */
		&xdg_activation_v1_interface,
		&xdg_activation_token_v1_interface,
		/* viewporter.xml */
		&wp_viewporter_interface,
		&wp_viewport_interface,
		/* fractional-scale-v1.xml */
		&wp_fractional_scale_manager_v1_interface,
		&wp_fractional_scale_v1_interface,
		/* xdg-toplevel-icon-v1.xml */
		&xdg_toplevel_icon_manager_v1_interface,
		&xdg_toplevel_icon_v1_interface,
		/* text-input-unstable-v3.xml */
		&zwp_text_input_manager_v3_interface,
		&zwp_text_input_v3_interface,
		/* input-method-unstable-v2.xml */
		&zwp_input_method_manager_v2_interface,
		&zwp_input_method_v2_interface,
		&zwp_input_popup_surface_v2_interface,
		&zwp_input_method_keyboard_grab_v2_interface,
		/* xdg-output-unstable-v1.xml */
		&zxdg_output_manager_v1_interface,
		&zxdg_output_v1_interface,
		/* presentation-time.xml */
		&wp_presentation_interface,
		&wp_presentation_feedback_interface,
		/* ext-idle-notify-v1.xml */
		&ext_idle_notifier_v1_interface,
		&ext_idle_notification_v1_interface,
		/* idle-inhibit-unstable-v1.xml */
		&zwp_idle_inhibit_manager_v1_interface,
		&zwp_idle_inhibitor_v1_interface,
		/* single-pixel-buffer-v1.xml */
		&wp_single_pixel_buffer_manager_v1_interface,
		/* content-type-v1.xml */
		&wp_content_type_manager_v1_interface,
		&wp_content_type_v1_interface,
		/* alpha-modifier-v1.xml */
		&wp_alpha_modifier_v1_interface,
		&wp_alpha_modifier_surface_v1_interface,
		/* xdg-dialog-v1.xml */
		&xdg_wm_dialog_v1_interface,
		&xdg_dialog_v1_interface,
		/* xdg-system-bell-v1.xml */
		&xdg_system_bell_v1_interface,
		/* xdg-toplevel-tag-v1.xml */
		&xdg_toplevel_tag_manager_v1_interface,
		/* kde-server-decoration.xml */
		&org_kde_kwin_server_decoration_manager_interface,
		&org_kde_kwin_server_decoration_interface,
		/* relative-pointer-unstable-v1.xml */
		&zwp_relative_pointer_manager_v1_interface,
		&zwp_relative_pointer_v1_interface,
		/* pointer-constraints-unstable-v1.xml */
		&zwp_pointer_constraints_v1_interface,
		&zwp_locked_pointer_v1_interface,
		&zwp_confined_pointer_v1_interface,
		/* pointer-gestures-unstable-v1.xml */
		&zwp_pointer_gestures_v1_interface,
		&zwp_pointer_gesture_swipe_v1_interface,
		&zwp_pointer_gesture_pinch_v1_interface,
		&zwp_pointer_gesture_hold_v1_interface,
		/* keyboard-shortcuts-inhibit-unstable-v1.xml */
		&zwp_keyboard_shortcuts_inhibit_manager_v1_interface,
		&zwp_keyboard_shortcuts_inhibitor_v1_interface,
		/* virtual-keyboard-unstable-v1.xml */
		&zwp_virtual_keyboard_v1_interface,
		&zwp_virtual_keyboard_manager_v1_interface,
		/* wlr-virtual-pointer-unstable-v1.xml */
		&zwlr_virtual_pointer_v1_interface,
		&zwlr_virtual_pointer_manager_v1_interface,
		/* ext-foreign-toplevel-list-v1.xml */
		&ext_foreign_toplevel_list_v1_interface,
		&ext_foreign_toplevel_handle_v1_interface,
		/* wlr-gamma-control-unstable-v1.xml */
		&zwlr_gamma_control_manager_v1_interface,
		&zwlr_gamma_control_v1_interface,
		/* wlr-output-power-management-unstable-v1.xml */
		&zwlr_output_power_manager_v1_interface,
		&zwlr_output_power_v1_interface,
		/* wlr-data-control-unstable-v1.xml */
		&zwlr_data_control_manager_v1_interface,
		&zwlr_data_control_device_v1_interface,
		&zwlr_data_control_source_v1_interface,
		&zwlr_data_control_offer_v1_interface,
		/* ext-data-control-v1.xml */
		&ext_data_control_manager_v1_interface,
		&ext_data_control_device_v1_interface,
		&ext_data_control_source_v1_interface,
		&ext_data_control_offer_v1_interface,
		/* wlr-output-management-unstable-v1.xml */
		&zwlr_output_manager_v1_interface,
		&zwlr_output_head_v1_interface,
		&zwlr_output_mode_v1_interface,
		&zwlr_output_configuration_v1_interface,
		&zwlr_output_configuration_head_v1_interface,
		/* ext-workspace-v1.xml */
		&ext_workspace_manager_v1_interface,
		&ext_workspace_group_handle_v1_interface,
		&ext_workspace_handle_v1_interface,
		/* hyprland-global-shortcuts-v1.xml */
		&hyprland_global_shortcuts_manager_v1_interface,
		&hyprland_global_shortcut_v1_interface,
		/* hyprland-focus-grab-v1.xml */
		&hyprland_focus_grab_manager_v1_interface,
		&hyprland_focus_grab_v1_interface,
		/* hyprland-lock-notify-v1.xml */
		&hyprland_lock_notifier_v1_interface,
		&hyprland_lock_notification_v1_interface,
		/* hyprland-toplevel-mapping-v1.xml */
		&hyprland_toplevel_mapping_manager_v1_interface,
		&hyprland_toplevel_window_mapping_handle_v1_interface,
		/* hyprland-surface-v1.xml */
		&hyprland_surface_manager_v1_interface,
		&hyprland_surface_v1_interface,
		/* hyprland-toplevel-export-v1.xml */
		&hyprland_toplevel_export_manager_v1_interface,
		&hyprland_toplevel_export_frame_v1_interface,
		/* pointer-warp-v1.xml */
		&wp_pointer_warp_v1_interface,
		/* ext-background-effect-v1.xml */
		&ext_background_effect_manager_v1_interface,
		&ext_background_effect_surface_v1_interface,
		/* tearing-control-v1.xml */
		&wp_tearing_control_manager_v1_interface,
		&wp_tearing_control_v1_interface,
		/* fifo-v1.xml */
		&wp_fifo_manager_v1_interface,
		&wp_fifo_v1_interface,
		/* commit-timing-v1.xml */
		&wp_commit_timing_manager_v1_interface,
		&wp_commit_timer_v1_interface,
		/* security-context-v1.xml */
		&wp_security_context_manager_v1_interface,
		&wp_security_context_v1_interface,
		/* vicinae-hotkey-v1.xml */
		&vicinae_hotkey_manager_v1_interface,
		&vicinae_hotkey_v1_interface,
		/* ext-image-capture-source-v1.xml */
		&ext_image_capture_source_v1_interface,
		&ext_output_image_capture_source_manager_v1_interface,
		&ext_foreign_toplevel_image_capture_source_manager_v1_interface,
		/* ext-image-copy-capture-v1.xml */
		&ext_image_copy_capture_manager_v1_interface,
		&ext_image_copy_capture_session_v1_interface,
		&ext_image_copy_capture_frame_v1_interface,
		&ext_image_copy_capture_cursor_session_v1_interface,
		/* linux-dmabuf-v1.xml */
		&zwp_linux_dmabuf_v1_interface,
		&zwp_linux_buffer_params_v1_interface,
		&zwp_linux_dmabuf_feedback_v1_interface,
	};

	printf("# libwayland %s\n", WAYLAND_VERSION);
	for (size_t i = 0; i < sizeof interfaces / sizeof interfaces[0]; i++)
		print_interface(interfaces[i]);
	return 0;
}
