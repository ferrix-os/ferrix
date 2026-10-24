//! The themes hyprix ships (`hyprix/themes/`, `docs/DESKTOP.md` §2) parse
//! with no diagnostic, and say what they are for.

use compositor_config::{NoSources, parse};

/// Ferrix light: every option and bind is one this compositor knows.
#[test]
fn ferrix_light_parses_and_floats() {
    let text = include_str!("../themes/ferrix-light.conf");
    let parsed = parse("ferrix-light.conf", text, &mut NoSources);
    assert_eq!(parsed.diagnostics, [], "the theme parses cleanly");
    let config = parsed.config;
    assert_eq!(config.str("general:layout"), Some("floating"));
    assert_eq!(config.int("hyprix:own_frames"), Some(1));
    let settings = compositor_layout::Settings::from_config(&config);
    assert_eq!(settings.layout, compositor_layout::Layout::Floating);
    for bind in &config.binds {
        // A key the keymap does not have is a bind the seat drops.
        if let compositor_config::Key::Sym(name) = &bind.key {
            assert!(
                compositor_xkb::code_of(name).is_some(),
                "{name} is a key of the default keymap"
            );
        }
        assert!(
            compositor_layout::Dispatcher::parse(&bind.dispatcher, &bind.arg).is_ok()
                || ["mouse", "overview", "switcher"].contains(&bind.dispatcher.as_str()),
            "{bind:?} names a dispatcher hyprix runs"
        );
    }
}
