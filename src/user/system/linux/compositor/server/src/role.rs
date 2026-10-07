//! What an object is, beyond which interface it speaks.
//!
//! The interface says how to decode a message; the role says what to do with
//! it. They are not the same thing -- two globals could one day share an
//! interface -- and keeping them apart means a handler never has to compare
//! interface pointers to find out what it is holding.

/// What a live object is to the compositor.
///
/// Carried in `wire::Objects`' per-object slot, so there is one map of live
/// objects and not two.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Role {
    /// `wl_display`, object 1 of every connection.
    Display,
    /// `wl_registry`, made by `wl_display.get_registry`.
    Registry,
    /// `wl_callback`, made by `wl_display.sync`. It is destroyed by the
    /// server the moment it has fired, which is why nothing else may be
    /// addressed to it.
    Callback,
    /// `wl_compositor`, bound from the registry.
    Compositor,
    /// `wl_subcompositor`.
    Subcompositor,
    /// `wl_shm`.
    Shm,
    /// `wl_seat`.
    Seat,
    /// `wl_output`.
    Output,
    /// `wl_data_device_manager`.
    DataDeviceManager,
    /// `xdg_wm_base`.
    XdgWmBase,
    /// `zxdg_decoration_manager_v1`.
    DecorationManager,
    /// `zxdg_toplevel_decoration_v1`: who draws one window's title bar.
    ToplevelDecoration,
    /// `zwlr_layer_shell_v1`.
    LayerShell,
    /// `zwlr_layer_surface_v1`: a bar, a wallpaper, a launcher.
    LayerSurface,
    /// `wl_surface`, made by `wl_compositor.create_surface`.
    Surface,
    /// `wl_region`, made by `wl_compositor.create_region`.
    Region,
    /// `wl_shm_pool`, made by `wl_shm.create_pool`.
    ShmPool,
    /// `wl_buffer`, made by `wl_shm_pool.create_buffer`.
    Buffer,
    /// A `wl_callback` a `wl_surface.frame` asked for. Unlike the one
    /// `wl_display.sync` makes, it lives until a frame is drawn.
    FrameCallback,
    /// `xdg_surface`, made by `xdg_wm_base.get_xdg_surface`.
    XdgSurface,
    /// `xdg_toplevel`: a window.
    XdgToplevel,
    /// `xdg_popup`: a menu anchored to another surface.
    XdgPopup,
    /// `xdg_positioner`, which says where a popup goes.
    XdgPositioner,
    /// `wl_subsurface`, made by `wl_subcompositor.get_subsurface`.
    Subsurface,
    /// `wl_pointer`, made by `wl_seat.get_pointer`.
    Pointer,
    /// `wl_keyboard`, made by `wl_seat.get_keyboard`.
    Keyboard,
    /// `wl_touch`, made by `wl_seat.get_touch`.
    Touch,
    /// `wl_data_device`, made by `wl_data_device_manager.get_data_device`.
    DataDevice,
    /// `wl_data_source`, made by `wl_data_device_manager.create_data_source`.
    DataSource,
    /// `wl_data_offer`, made by the *server* when it tells a client what the
    /// selection holds.
    DataOffer,
    /// `zwlr_foreign_toplevel_manager_v1`: a bar's list of windows.
    ForeignToplevelManager,
    /// `zwlr_foreign_toplevel_handle_v1`: one window in that list, made by
    /// the *server* -- the manager does not ask for them, it is told.
    ForeignToplevel,
    /// `zwlr_screencopy_manager_v1`: a screenshot program.
    ScreencopyManager,
    /// `zwlr_screencopy_frame_v1`: one screenshot being taken.
    ScreencopyFrame,
    /// `ext_session_lock_manager_v1`: a program that can lock the screen.
    SessionLockManager,
    /// `ext_session_lock_v1`: the lock itself, while it is held.
    SessionLock,
    /// `ext_session_lock_surface_v1`: what is shown on one screen while the
    /// session is locked.
    SessionLockSurface,
    /// `wp_cursor_shape_manager_v1`: a client naming its cursor.
    CursorShapeManager,
    /// `wp_cursor_shape_device_v1`: one pointer's named cursor.
    CursorShapeDevice,
    /// `zwp_primary_selection_device_manager_v1`: the middle-click paste.
    PrimaryManager,
    /// `zwp_primary_selection_device_v1`.
    PrimaryDevice,
    /// `zwp_primary_selection_source_v1`: what a client selected.
    PrimarySource,
    /// `zwp_primary_selection_offer_v1`, made by the *server*.
    PrimaryOffer,
    /// `xdg_activation_v1`: one program asking for another to be focused.
    Activation,
    /// `xdg_activation_token_v1`: one such request being made.
    ActivationToken,
    /// `wp_viewporter`.
    Viewporter,
    /// `wp_viewport`: one surface's crop and scale.
    Viewport,
    /// `wp_fractional_scale_manager_v1`.
    FractionalScaleManager,
    /// `wp_fractional_scale_v1`: one surface's preferred scale.
    FractionalScale,
    /// `xdg_toplevel_icon_manager_v1`.
    IconManager,
    /// `xdg_toplevel_icon_v1`: one window's icon.
    Icon,
    /// `zwp_text_input_manager_v3`: an application that wants to be typed
    /// into through an input method.
    TextInputManager,
    /// `zwp_text_input_v3`: one such application's text field.
    TextInput,
    /// `zwp_input_method_manager_v2`: the input method's own side.
    InputMethodManager,
    /// `zwp_input_method_v2`: the input method itself.
    InputMethod,
    /// `zwp_input_popup_surface_v2`: the candidate window an input method
    /// shows beside the text being typed.
    InputPopup,
    /// `zwp_input_method_keyboard_grab_v2`: the keyboard while an input
    /// method has it.
    InputGrab,
    /// `zxdg_output_manager_v1`: a screen in the logical pixels a bar lays
    /// itself out in.
    XdgOutputManager,
    /// `zxdg_output_v1`: one screen's.
    XdgOutput,
    /// `wp_presentation`: when a frame reached the screen.
    Presentation,
    /// `wp_presentation_feedback`: one frame's answer, made by the client
    /// and destroyed by the event that answers it.
    PresentationFeedback,
    /// `ext_idle_notifier_v1`: a screen locker or a power daemon.
    IdleNotifier,
    /// `ext_idle_notification_v1`: one such timeout.
    IdleNotification,
    /// `zwp_idle_inhibit_manager_v1`: a video player holding it off.
    IdleInhibitManager,
    /// `zwp_idle_inhibitor_v1`: one such hold.
    IdleInhibitor,
    /// `wp_single_pixel_buffer_manager_v1`: a buffer that is one colour.
    SinglePixelManager,
    /// `zwp_linux_dmabuf_v1`: buffers that live on the GPU.
    LinuxDmabuf,
    /// `zwp_linux_buffer_params_v1`: one such buffer being described.
    BufferParams,
    /// `wp_content_type_manager_v1`: what a surface is showing.
    ContentTypeManager,
    /// `wp_content_type_v1`: one surface's.
    ContentType,
    /// `wp_alpha_modifier_v1`: a surface asking to be drawn see-through.
    AlphaModifier,
    /// `wp_alpha_modifier_surface_v1`: one surface's.
    AlphaSurface,
    /// `xdg_wm_dialog_v1`: a dialog saying it is modal.
    DialogManager,
    /// `xdg_dialog_v1`: one dialog.
    Dialog,
    /// `xdg_system_bell_v1`: the terminal bell.
    SystemBell,
    /// `xdg_toplevel_tag_manager_v1`: a name a window keeps across
    /// restarts.
    ToplevelTagManager,
    /// `org_kde_kwin_server_decoration_manager`: KDE's `xdg-decoration`.
    KdeDecorationManager,
    /// `org_kde_kwin_server_decoration`: one surface's.
    KdeDecoration,
    /// `zwp_relative_pointer_manager_v1`: how far the pointer moved, for a
    /// client that does not care where it is.
    RelativePointerManager,
    /// `zwp_relative_pointer_v1`: one pointer's.
    RelativePointer,
    /// `zwp_pointer_constraints_v1`: keeping the pointer in a window.
    PointerConstraints,
    /// `zwp_locked_pointer_v1`: the pointer held still.
    LockedPointer,
    /// `zwp_confined_pointer_v1`: the pointer held inside a surface.
    ConfinedPointer,
    /// `zwp_keyboard_shortcuts_inhibit_manager_v1`: a virtual machine or a
    /// nested compositor asking for `SUPER`.
    ShortcutsInhibitManager,
    /// `zwp_keyboard_shortcuts_inhibitor_v1`: one such request.
    ShortcutsInhibitor,
    /// `zwp_virtual_keyboard_manager_v1`: a client acting as a keyboard.
    VirtualKeyboardManager,
    /// `zwp_virtual_keyboard_v1`: one such keyboard.
    VirtualKeyboard,
    /// `zwlr_virtual_pointer_manager_v1`: a client acting as a mouse.
    VirtualPointerManager,
    /// `zwlr_virtual_pointer_v1`: one such mouse.
    VirtualPointer,
    /// `zwp_pointer_gestures_v1`: a touchpad's swipe, pinch and hold.
    PointerGestures,
    /// `zwp_pointer_gesture_swipe_v1`.
    GestureSwipe,
    /// `zwp_pointer_gesture_pinch_v1`.
    GesturePinch,
    /// `zwp_pointer_gesture_hold_v1`.
    GestureHold,
    /// `ext_foreign_toplevel_list_v1`: the window list as the newer
    /// specification has it.
    ForeignList,
    /// `ext_foreign_toplevel_handle_v1`: one window in it, made by the
    /// *server*.
    ForeignListHandle,
    /// `zwlr_gamma_control_manager_v1`: a night-light.
    GammaControlManager,
    /// `zwlr_gamma_control_v1`: one screen's ramps.
    GammaControl,
    /// `zwlr_output_power_manager_v1`: `wlopm`.
    OutputPowerManager,
    /// `zwlr_output_power_v1`: one screen's power.
    OutputPower,
    /// `zwlr_data_control_manager_v1` or `ext_data_control_manager_v1`: a
    /// clipboard manager, which has no window and is told anyway.
    DataControlManager(crate::client::Flavour),
    /// The device either makes.
    DataControlDevice,
    /// The source either makes.
    DataControlSource,
    /// The offer the *server* makes for either.
    DataControlOffer,
    /// `zwlr_output_manager_v1`: `kanshi` and `wlr-randr`.
    OutputManager,
    /// `zwlr_output_head_v1`: one screen, made by the server.
    OutputHead,
    /// `zwlr_output_mode_v1`: one of its modes, the same.
    OutputMode,
    /// `zwlr_output_configuration_v1`: an arrangement being built.
    OutputConfiguration,
    /// `zwlr_output_configuration_head_v1`: one screen in it.
    OutputConfigurationHead,
    /// `ext_workspace_manager_v1`: the workspace numbers a bar draws.
    WorkspaceManager,
    /// `ext_workspace_group_handle_v1`: one monitor's workspaces.
    WorkspaceGroup,
    /// `ext_workspace_handle_v1`: one workspace.
    WorkspaceHandle,
    /// `hyprland_global_shortcuts_manager_v1`: a shortcut a program
    /// registers rather than a keybind.
    GlobalShortcuts,
    /// `hyprland_global_shortcut_v1`: one of them.
    GlobalShortcut,
    /// `hyprland_focus_grab_manager_v1`: a launcher holding the focus.
    FocusGrabManager,
    /// `hyprland_focus_grab_v1`: one such hold.
    FocusGrab,
    /// `hyprland_lock_notifier_v1`: a program told when the screen locks.
    LockNotifier,
    /// `hyprland_lock_notification_v1`: one such request.
    LockNotification,
    /// `hyprland_toplevel_mapping_manager_v1`: the address every other
    /// protocol calls a window by.
    ToplevelMapping,
    /// `hyprland_toplevel_window_mapping_handle_v1`: one answer.
    MappingHandle,
    /// `hyprland_surface_manager_v1`: a surface's own opacity.
    HyprlandSurfaceManager,
    /// `hyprland_surface_v1`: one surface's.
    HyprlandSurface,
    /// `hyprland_toplevel_export_manager_v1`: a screenshot of one window.
    ToplevelExportManager,
    /// `hyprland_toplevel_export_frame_v1`: one being taken.
    ToplevelExportFrame,
    /// `wp_pointer_warp_v1`: a client putting the pointer inside its own
    /// window.
    PointerWarp,
    /// `ext_background_effect_manager_v1`: a surface asking for what is
    /// behind it to be blurred.
    BackgroundEffectManager,
    /// `ext_background_effect_surface_v1`: one surface's.
    BackgroundEffect,
    /// `wp_tearing_control_manager_v1`: how a client would like its frames
    /// put on the screen.
    TearingManager,
    /// `wp_tearing_control_v1`: one surface's.
    Tearing,
    /// `wp_fifo_manager_v1`: a client asking for its frames in order.
    FifoManager,
    /// `wp_fifo_v1`: one surface's.
    Fifo,
    /// `wp_commit_timing_manager_v1`: a client asking for a commit at a
    /// time.
    CommitTimingManager,
    /// `wp_commit_timer_v1`: one surface's.
    CommitTimer,
    /// `wp_security_context_manager_v1`: a sandbox asking for a socket of
    /// its own.
    SecurityContextManager,
    /// `wp_security_context_v1`: one sandbox.
    SecurityContext,
    /// `vicinae_hotkey_manager_v1`: a launcher asking for a key by keysym.
    HotkeyManager,
    /// `vicinae_hotkey_v1`: one such key.
    Hotkey,
    /// `ext_output_image_capture_source_manager_v1`: a screen as something
    /// that can be captured.
    OutputCaptureSourceManager,
    /// `ext_foreign_toplevel_image_capture_source_manager_v1`: a window as
    /// the same.
    ToplevelCaptureSourceManager,
    /// `ext_image_capture_source_v1`: one such thing.
    CaptureSource,
    /// `ext_image_copy_capture_manager_v1`: a recorder or a portal.
    CaptureManager,
    /// `ext_image_copy_capture_session_v1`: frames out of one source.
    CaptureSession,
    /// `ext_image_copy_capture_frame_v1`: one of them.
    CaptureFrame,
    /// `ext_image_copy_capture_cursor_session_v1`: the cursor by itself,
    /// which this compositor draws into the frame instead.
    CursorCaptureSession,
}

impl Role {
    /// Whether a client may address requests to an object of this role.
    ///
    /// `wl_callback` is the one that cannot: the protocol gives it no
    /// requests at all, and a client that sends one to it has either lost
    /// track of an id the server already took back or is guessing.
    #[must_use]
    pub const fn takes_requests(self) -> bool {
        !matches!(self, Self::Callback | Self::FrameCallback)
    }
}
