/*
 * The DRM/KMS numbers and layouts `src/lib/proto/linux-abi/src/drm.rs` writes down,
 * printed from the UAPI headers themselves.
 *
 * Run by `src/lib/proto/linux-abi/probe/drm.sh` on a Linux host with the kernel's
 * UAPI headers (`linux-libc-dev`) once natively for 64-bit and once for
 * ARMv7-A under qemu-arm, into `drm-64.txt` and `drm-32.txt` beside it. The
 * crate's tests read both files and require every line to match what the
 * crate says, so a number changed on either side fails `cargo test`.
 *
 * Each line is `name value`, in decimal. Layouts are `sizeof.<struct>` and
 * `offsetof.<struct>.<field>`.
 */

#include <stddef.h>
#include <stdio.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>
#include <drm/drm_fourcc.h>

#define VALUE(name) printf("%s %llu\n", #name, (unsigned long long)(name))
#define SIZE(type) printf("sizeof.%s %zu\n", #type, sizeof(struct type))
#define FIELD(type, field) \
	printf("offsetof.%s.%s %zu\n", #type, #field, offsetof(struct type, field))

int main(void)
{
	/* The ioctls iteration 1 of the display answers. */
	VALUE(DRM_IOCTL_VERSION);
	VALUE(DRM_IOCTL_GET_CAP);
	VALUE(DRM_IOCTL_SET_CLIENT_CAP);
	VALUE(DRM_IOCTL_SET_MASTER);
	VALUE(DRM_IOCTL_DROP_MASTER);
	VALUE(DRM_IOCTL_MODE_GETRESOURCES);
	VALUE(DRM_IOCTL_MODE_GETCRTC);
	VALUE(DRM_IOCTL_MODE_SETCRTC);
	VALUE(DRM_IOCTL_MODE_GETENCODER);
	VALUE(DRM_IOCTL_MODE_GETCONNECTOR);
	VALUE(DRM_IOCTL_MODE_ADDFB);
	VALUE(DRM_IOCTL_MODE_ADDFB2);
	VALUE(DRM_IOCTL_MODE_RMFB);
	VALUE(DRM_IOCTL_MODE_PAGE_FLIP);
	VALUE(DRM_IOCTL_MODE_DIRTYFB);
	/* The cursor plane, as the legacy calls set it: an image and a place. */
	VALUE(DRM_IOCTL_MODE_CURSOR);
	VALUE(DRM_IOCTL_MODE_CURSOR2);
	VALUE(DRM_IOCTL_MODE_CREATE_DUMB);
	VALUE(DRM_IOCTL_MODE_MAP_DUMB);
	VALUE(DRM_IOCTL_MODE_DESTROY_DUMB);
	/* Letting a handle go, and passing a buffer object between two nodes
	 * of one card: what a compositor drawing on the render node and
	 * showing on the card needs (docs/GPU.md step 2 and piece 6).
	 *
	 * `DRM_CLOEXEC` and `DRM_RDWR`, which are the flags a handle is
	 * exported with, are `O_CLOEXEC` and `O_RDWR` and are probed with the
	 * rest of `fcntl.h`'s numbers rather than here. */
	VALUE(DRM_IOCTL_GEM_CLOSE);
	VALUE(DRM_IOCTL_PRIME_HANDLE_TO_FD);
	VALUE(DRM_IOCTL_PRIME_FD_TO_HANDLE);

	/* Planes and properties, iteration 2 (E4). */
	VALUE(DRM_IOCTL_MODE_GETPLANERESOURCES);
	VALUE(DRM_IOCTL_MODE_GETPLANE);
	VALUE(DRM_IOCTL_MODE_OBJ_GETPROPERTIES);
	VALUE(DRM_IOCTL_MODE_GETPROPERTY);
	VALUE(DRM_IOCTL_MODE_GETPROPBLOB);

	/* Capabilities, and the client capabilities a client may ask for. */
	VALUE(DRM_CAP_DUMB_BUFFER);
	VALUE(DRM_CAP_VBLANK_HIGH_CRTC);
	VALUE(DRM_CAP_DUMB_PREFERRED_DEPTH);
	VALUE(DRM_CAP_DUMB_PREFER_SHADOW);
	VALUE(DRM_CAP_PRIME);
	VALUE(DRM_PRIME_CAP_IMPORT);
	VALUE(DRM_PRIME_CAP_EXPORT);
	VALUE(DRM_CAP_TIMESTAMP_MONOTONIC);
	VALUE(DRM_CAP_ASYNC_PAGE_FLIP);
	VALUE(DRM_CAP_CURSOR_WIDTH);
	VALUE(DRM_CAP_CURSOR_HEIGHT);
	VALUE(DRM_CAP_ADDFB2_MODIFIERS);
	VALUE(DRM_CAP_PAGE_FLIP_TARGET);
	VALUE(DRM_CAP_CRTC_IN_VBLANK_EVENT);
	VALUE(DRM_CAP_SYNCOBJ);
	VALUE(DRM_CAP_SYNCOBJ_TIMELINE);
	VALUE(DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP);
	VALUE(DRM_CLIENT_CAP_STEREO_3D);
	VALUE(DRM_CLIENT_CAP_UNIVERSAL_PLANES);
	VALUE(DRM_CLIENT_CAP_ATOMIC);
	VALUE(DRM_CLIENT_CAP_ASPECT_RATIO);
	VALUE(DRM_CLIENT_CAP_WRITEBACK_CONNECTORS);
	VALUE(DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT);

	/* Events read from the card. */
	VALUE(DRM_EVENT_VBLANK);
	VALUE(DRM_EVENT_FLIP_COMPLETE);
	VALUE(DRM_EVENT_CRTC_SEQUENCE);

	/* Modes, connectors and encoders. */
	VALUE(DRM_DISPLAY_MODE_LEN);
	VALUE(DRM_MODE_TYPE_PREFERRED);
	VALUE(DRM_MODE_TYPE_USERDEF);
	VALUE(DRM_MODE_TYPE_DRIVER);
	VALUE(DRM_MODE_FLAG_PHSYNC);
	VALUE(DRM_MODE_FLAG_NHSYNC);
	VALUE(DRM_MODE_FLAG_PVSYNC);
	VALUE(DRM_MODE_FLAG_NVSYNC);
	VALUE(DRM_MODE_CONNECTOR_Unknown);
	VALUE(DRM_MODE_CONNECTOR_VIRTUAL);
	VALUE(DRM_MODE_ENCODER_NONE);
	VALUE(DRM_MODE_ENCODER_VIRTUAL);

	/* Framebuffers and page flips. */
	VALUE(DRM_MODE_FB_INTERLACED);
	VALUE(DRM_MODE_FB_MODIFIERS);
	VALUE(DRM_MODE_PAGE_FLIP_EVENT);
	VALUE(DRM_MODE_PAGE_FLIP_ASYNC);
	VALUE(DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE);
	VALUE(DRM_MODE_PAGE_FLIP_TARGET_RELATIVE);
	VALUE(DRM_MODE_FB_DIRTY_MAX_CLIPS);
	VALUE(DRM_MODE_CURSOR_BO);
	VALUE(DRM_MODE_CURSOR_MOVE);
	VALUE(DRM_MODE_CURSOR_FLAGS);
	VALUE(DRM_FORMAT_XRGB8888);
	VALUE(DRM_FORMAT_ARGB8888);
	VALUE(DRM_FORMAT_XBGR8888);
	VALUE(DRM_FORMAT_ABGR8888);

	/* Mode objects and properties. */
	VALUE(DRM_MODE_OBJECT_CRTC);
	VALUE(DRM_MODE_OBJECT_CONNECTOR);
	VALUE(DRM_MODE_OBJECT_ENCODER);
	VALUE(DRM_MODE_OBJECT_MODE);
	VALUE(DRM_MODE_OBJECT_PROPERTY);
	VALUE(DRM_MODE_OBJECT_FB);
	VALUE(DRM_MODE_OBJECT_BLOB);
	VALUE(DRM_MODE_OBJECT_PLANE);
	VALUE(DRM_MODE_OBJECT_ANY);
	VALUE(DRM_PROP_NAME_LEN);
	VALUE(DRM_MODE_PROP_PENDING);
	VALUE(DRM_MODE_PROP_RANGE);
	VALUE(DRM_MODE_PROP_IMMUTABLE);
	VALUE(DRM_MODE_PROP_ENUM);
	VALUE(DRM_MODE_PROP_BLOB);
	VALUE(DRM_MODE_PROP_BITMASK);
	VALUE(DRM_MODE_PROP_LEGACY_TYPE);
	VALUE(DRM_MODE_PROP_EXTENDED_TYPE);
	VALUE(DRM_MODE_PROP_OBJECT);
	VALUE(DRM_MODE_PROP_SIGNED_RANGE);
	VALUE(DRM_MODE_PROP_ATOMIC);

	/* Layouts. */
	SIZE(drm_version);
	FIELD(drm_version, version_major);
	FIELD(drm_version, version_minor);
	FIELD(drm_version, version_patchlevel);
	FIELD(drm_version, name_len);
	FIELD(drm_version, name);
	FIELD(drm_version, date_len);
	FIELD(drm_version, date);
	FIELD(drm_version, desc_len);
	FIELD(drm_version, desc);

	SIZE(drm_get_cap);
	FIELD(drm_get_cap, capability);
	FIELD(drm_get_cap, value);

	SIZE(drm_set_client_cap);
	FIELD(drm_set_client_cap, capability);
	FIELD(drm_set_client_cap, value);

	SIZE(drm_mode_modeinfo);
	FIELD(drm_mode_modeinfo, clock);
	FIELD(drm_mode_modeinfo, hdisplay);
	FIELD(drm_mode_modeinfo, hsync_start);
	FIELD(drm_mode_modeinfo, hsync_end);
	FIELD(drm_mode_modeinfo, htotal);
	FIELD(drm_mode_modeinfo, hskew);
	FIELD(drm_mode_modeinfo, vdisplay);
	FIELD(drm_mode_modeinfo, vsync_start);
	FIELD(drm_mode_modeinfo, vsync_end);
	FIELD(drm_mode_modeinfo, vtotal);
	FIELD(drm_mode_modeinfo, vscan);
	FIELD(drm_mode_modeinfo, vrefresh);
	FIELD(drm_mode_modeinfo, flags);
	FIELD(drm_mode_modeinfo, type);
	FIELD(drm_mode_modeinfo, name);

	SIZE(drm_mode_card_res);
	FIELD(drm_mode_card_res, fb_id_ptr);
	FIELD(drm_mode_card_res, crtc_id_ptr);
	FIELD(drm_mode_card_res, connector_id_ptr);
	FIELD(drm_mode_card_res, encoder_id_ptr);
	FIELD(drm_mode_card_res, count_fbs);
	FIELD(drm_mode_card_res, count_crtcs);
	FIELD(drm_mode_card_res, count_connectors);
	FIELD(drm_mode_card_res, count_encoders);
	FIELD(drm_mode_card_res, min_width);
	FIELD(drm_mode_card_res, max_width);
	FIELD(drm_mode_card_res, min_height);
	FIELD(drm_mode_card_res, max_height);

	SIZE(drm_mode_crtc);
	FIELD(drm_mode_crtc, set_connectors_ptr);
	FIELD(drm_mode_crtc, count_connectors);
	FIELD(drm_mode_crtc, crtc_id);
	FIELD(drm_mode_crtc, fb_id);
	FIELD(drm_mode_crtc, x);
	FIELD(drm_mode_crtc, y);
	FIELD(drm_mode_crtc, gamma_size);
	FIELD(drm_mode_crtc, mode_valid);
	FIELD(drm_mode_crtc, mode);

	SIZE(drm_mode_get_encoder);
	FIELD(drm_mode_get_encoder, encoder_id);
	FIELD(drm_mode_get_encoder, encoder_type);
	FIELD(drm_mode_get_encoder, crtc_id);
	FIELD(drm_mode_get_encoder, possible_crtcs);
	FIELD(drm_mode_get_encoder, possible_clones);

	SIZE(drm_mode_get_connector);
	FIELD(drm_mode_get_connector, encoders_ptr);
	FIELD(drm_mode_get_connector, modes_ptr);
	FIELD(drm_mode_get_connector, props_ptr);
	FIELD(drm_mode_get_connector, prop_values_ptr);
	FIELD(drm_mode_get_connector, count_modes);
	FIELD(drm_mode_get_connector, count_props);
	FIELD(drm_mode_get_connector, count_encoders);
	FIELD(drm_mode_get_connector, encoder_id);
	FIELD(drm_mode_get_connector, connector_id);
	FIELD(drm_mode_get_connector, connector_type);
	FIELD(drm_mode_get_connector, connector_type_id);
	FIELD(drm_mode_get_connector, connection);
	FIELD(drm_mode_get_connector, mm_width);
	FIELD(drm_mode_get_connector, mm_height);
	FIELD(drm_mode_get_connector, subpixel);
	FIELD(drm_mode_get_connector, pad);

	SIZE(drm_mode_fb_cmd);
	FIELD(drm_mode_fb_cmd, fb_id);
	FIELD(drm_mode_fb_cmd, width);
	FIELD(drm_mode_fb_cmd, height);
	FIELD(drm_mode_fb_cmd, pitch);
	FIELD(drm_mode_fb_cmd, bpp);
	FIELD(drm_mode_fb_cmd, depth);
	FIELD(drm_mode_fb_cmd, handle);

	SIZE(drm_mode_fb_cmd2);
	FIELD(drm_mode_fb_cmd2, fb_id);
	FIELD(drm_mode_fb_cmd2, width);
	FIELD(drm_mode_fb_cmd2, height);
	FIELD(drm_mode_fb_cmd2, pixel_format);
	FIELD(drm_mode_fb_cmd2, flags);
	FIELD(drm_mode_fb_cmd2, handles);
	FIELD(drm_mode_fb_cmd2, pitches);
	FIELD(drm_mode_fb_cmd2, offsets);
	FIELD(drm_mode_fb_cmd2, modifier);

	SIZE(drm_mode_crtc_page_flip);
	FIELD(drm_mode_crtc_page_flip, crtc_id);
	FIELD(drm_mode_crtc_page_flip, fb_id);
	FIELD(drm_mode_crtc_page_flip, flags);
	FIELD(drm_mode_crtc_page_flip, reserved);
	FIELD(drm_mode_crtc_page_flip, user_data);

	SIZE(drm_mode_fb_dirty_cmd);
	FIELD(drm_mode_fb_dirty_cmd, fb_id);
	FIELD(drm_mode_fb_dirty_cmd, flags);
	FIELD(drm_mode_fb_dirty_cmd, color);
	FIELD(drm_mode_fb_dirty_cmd, num_clips);
	FIELD(drm_mode_fb_dirty_cmd, clips_ptr);

	SIZE(drm_mode_cursor);
	FIELD(drm_mode_cursor, flags);
	FIELD(drm_mode_cursor, crtc_id);
	FIELD(drm_mode_cursor, x);
	FIELD(drm_mode_cursor, y);
	FIELD(drm_mode_cursor, width);
	FIELD(drm_mode_cursor, height);
	FIELD(drm_mode_cursor, handle);

	SIZE(drm_mode_cursor2);
	FIELD(drm_mode_cursor2, flags);
	FIELD(drm_mode_cursor2, crtc_id);
	FIELD(drm_mode_cursor2, x);
	FIELD(drm_mode_cursor2, y);
	FIELD(drm_mode_cursor2, width);
	FIELD(drm_mode_cursor2, height);
	FIELD(drm_mode_cursor2, handle);
	FIELD(drm_mode_cursor2, hot_x);
	FIELD(drm_mode_cursor2, hot_y);

	SIZE(drm_clip_rect);
	FIELD(drm_clip_rect, x1);
	FIELD(drm_clip_rect, y1);
	FIELD(drm_clip_rect, x2);
	FIELD(drm_clip_rect, y2);

	SIZE(drm_mode_create_dumb);
	FIELD(drm_mode_create_dumb, height);
	FIELD(drm_mode_create_dumb, width);
	FIELD(drm_mode_create_dumb, bpp);
	FIELD(drm_mode_create_dumb, flags);
	FIELD(drm_mode_create_dumb, handle);
	FIELD(drm_mode_create_dumb, pitch);
	FIELD(drm_mode_create_dumb, size);

	SIZE(drm_mode_map_dumb);
	FIELD(drm_mode_map_dumb, handle);
	FIELD(drm_mode_map_dumb, pad);
	FIELD(drm_mode_map_dumb, offset);

	SIZE(drm_mode_destroy_dumb);
	FIELD(drm_mode_destroy_dumb, handle);

	SIZE(drm_mode_get_plane_res);
	FIELD(drm_mode_get_plane_res, plane_id_ptr);
	FIELD(drm_mode_get_plane_res, count_planes);

	SIZE(drm_mode_get_plane);
	FIELD(drm_mode_get_plane, plane_id);
	FIELD(drm_mode_get_plane, crtc_id);
	FIELD(drm_mode_get_plane, fb_id);
	FIELD(drm_mode_get_plane, possible_crtcs);
	FIELD(drm_mode_get_plane, gamma_size);
	FIELD(drm_mode_get_plane, count_format_types);
	FIELD(drm_mode_get_plane, format_type_ptr);

	SIZE(drm_mode_obj_get_properties);
	FIELD(drm_mode_obj_get_properties, props_ptr);
	FIELD(drm_mode_obj_get_properties, prop_values_ptr);
	FIELD(drm_mode_obj_get_properties, count_props);
	FIELD(drm_mode_obj_get_properties, obj_id);
	FIELD(drm_mode_obj_get_properties, obj_type);

	SIZE(drm_mode_get_blob);
	FIELD(drm_mode_get_blob, blob_id);
	FIELD(drm_mode_get_blob, length);
	FIELD(drm_mode_get_blob, data);

	SIZE(drm_mode_get_property);
	FIELD(drm_mode_get_property, values_ptr);
	FIELD(drm_mode_get_property, enum_blob_ptr);
	FIELD(drm_mode_get_property, prop_id);
	FIELD(drm_mode_get_property, flags);
	FIELD(drm_mode_get_property, name);
	FIELD(drm_mode_get_property, count_values);
	FIELD(drm_mode_get_property, count_enum_blobs);

	SIZE(drm_mode_property_enum);
	FIELD(drm_mode_property_enum, value);
	FIELD(drm_mode_property_enum, name);

	SIZE(drm_gem_close);
	FIELD(drm_gem_close, handle);
	FIELD(drm_gem_close, pad);

	SIZE(drm_prime_handle);
	FIELD(drm_prime_handle, handle);
	FIELD(drm_prime_handle, flags);
	FIELD(drm_prime_handle, fd);

	SIZE(drm_event);
	FIELD(drm_event, type);
	FIELD(drm_event, length);

	SIZE(drm_event_vblank);
	FIELD(drm_event_vblank, base);
	FIELD(drm_event_vblank, user_data);
	FIELD(drm_event_vblank, tv_sec);
	FIELD(drm_event_vblank, tv_usec);
	FIELD(drm_event_vblank, sequence);
	FIELD(drm_event_vblank, crtc_id);
	return 0;
}
