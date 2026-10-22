use std::collections::BTreeMap;

use crate::options::OPTIONS;
use crate::parse::{MAX_SOURCE_DEPTH, MAX_SOURCED_FILES, MAX_VALUE_LEN};
use crate::{
    Bind, BindFlags, Color, Config, Diagnostic, FsSources, Gaps, Gradient, Key, Mods, NoSources,
    OptionValue, Parsed, SourceFile, Sources, parse, parse_color, parse_float, parse_gaps,
    parse_gradient, parse_int,
};

/// Sources from memory, by exact name.
#[derive(Default)]
struct Memory(BTreeMap<String, String>);

impl Sources for Memory {
    fn resolve(&mut self, spec: &str, _from: &str) -> Result<Vec<SourceFile>, String> {
        self.0
            .get(spec)
            .map(|text| {
                vec![SourceFile {
                    name: spec.to_owned(),
                    text: text.clone(),
                }]
            })
            .ok_or_else(|| format!("source file {spec} not found"))
    }
}

fn clean(text: &str) -> Config {
    let Parsed {
        config,
        diagnostics,
    } = parse("hyprland.conf", text, &mut NoSources);
    assert_eq!(diagnostics, [], "the configuration parses without errors");
    config
}

fn messages(text: &str) -> Vec<(usize, String)> {
    parse("hyprland.conf", text, &mut NoSources)
        .diagnostics
        .into_iter()
        .map(|diagnostic| (diagnostic.line, diagnostic.message))
        .collect()
}

// -- The option table ---------------------------------------------------------

#[test]
fn the_option_table_is_sorted_unique_and_every_default_parses_back() {
    for pair in OPTIONS.windows(2) {
        if let [(first, _), (second, _)] = pair {
            assert!(first < second, "{first} sorts before {second}");
        }
    }
    let config = Config::default();
    for &(name, default) in OPTIONS {
        let value = config.option(name).expect("every table entry has a value");
        assert_eq!(*value, default.value());
        let written = value.to_string();
        assert_eq!(
            default.parse(&written).as_ref(),
            Ok(value),
            "{name}'s default written as {written:?} parses back"
        );
    }
}

#[test]
fn defaults_are_hyprlands() {
    let config = Config::default();
    assert_eq!(config.int("general:border_size"), Some(1));
    assert_eq!(config.gaps("general:gaps_in"), Some(Gaps::all(5)));
    assert_eq!(config.gaps("general:gaps_out"), Some(Gaps::all(20)));
    assert_eq!(config.str("general:layout"), Some("dwindle"));
    assert_eq!(
        config.gradient("general:col.active_border"),
        Some(&Gradient::solid(Color(0xffff_ffff)))
    );
    assert_eq!(config.float("master:mfact"), Some(0.55));
    assert_eq!(config.bool("dwindle:use_active_for_splits"), Some(true));
    assert_eq!(config.int("input:repeat_rate"), Some(25));
    assert_eq!(config.int("general:layout"), None, "a string is not an int");
    assert_eq!(config.option("general:nonexistent"), None);
}

// -- Values ------------------------------------------------------------------------

#[test]
fn integers_take_decimal_hex_colours_and_boolean_prefixes() {
    assert_eq!(parse_int("42"), Ok(42));
    assert_eq!(parse_int(" -7 "), Ok(-7));
    assert_eq!(parse_int("0xff"), Ok(255));
    assert_eq!(parse_int("true"), Ok(1));
    assert_eq!(parse_int("on"), Ok(1));
    assert_eq!(parse_int("yes"), Ok(1));
    assert_eq!(parse_int("yesterday"), Ok(1), "hyprlang matches a prefix");
    assert_eq!(parse_int("false"), Ok(0));
    assert_eq!(parse_int("off"), Ok(0));
    assert_eq!(parse_int("nothing"), Ok(0), "hyprlang matches a prefix");
    assert_eq!(
        parse_int("5px"),
        Err("cannot parse \"5px\" as an int.".to_owned())
    );
    assert!(parse_int("").is_err());
    assert!(parse_int("-").is_err());
    assert!(parse_int("1.5").is_err());
    assert!(parse_int("99999999999999999999").is_err());
}

#[test]
fn colours_in_every_form_hyprland_writes() {
    assert_eq!(parse_color("rgba(33ccffee)"), Ok(Color(0xee33_ccff)));
    assert_eq!(parse_color("rgb(33ccff)"), Ok(Color(0xff33_ccff)));
    assert_eq!(parse_color("0xee33ccff"), Ok(Color(0xee33_ccff)));
    assert_eq!(
        parse_color("rgba(255, 0, 128, 0.5)"),
        Ok(Color(0x80ff_0080))
    );
    assert_eq!(parse_color("rgba(255,0,128,1)"), Ok(Color(0xffff_0080)));
    assert_eq!(parse_color("rgb(1, 2, 3)"), Ok(Color(0xff01_0203)));

    let color = Color(0x1122_3344);
    assert_eq!(
        (color.alpha(), color.red(), color.green(), color.blue()),
        (0x11, 0x22, 0x33, 0x44)
    );

    assert!(parse_color("rgba(33ccff)").is_err(), "six digits in rgba");
    assert!(parse_color("rgb(33ccffee)").is_err(), "eight digits in rgb");
    assert!(parse_color("rgba(256, 0, 0, 1)").is_err());
    assert!(parse_color("rgba(0, 0, 0, 1.5)").is_err());
    assert!(parse_color("rgba(gggggggg)").is_err());
    assert!(parse_color("-1").is_err());
    assert!(parse_color("0x1ffffffff").is_err(), "wider than 32 bits");
}

#[test]
fn floats() {
    assert_eq!(parse_float("0.55"), Ok(0.55));
    assert_eq!(parse_float("-1"), Ok(-1.0));
    assert_eq!(parse_float(".5"), Ok(0.5));
    assert!(parse_float("1.2.3").is_err());
    assert!(parse_float(".").is_err());
    assert!(parse_float("inf").is_err());
    assert!(parse_float("1e3").is_err());
}

#[test]
fn gradients_take_up_to_ten_colours_and_an_angle() {
    assert_eq!(
        parse_gradient("rgba(33ccffee) rgba(00ff99ee) 45deg"),
        Ok(Gradient {
            colors: vec![Color(0xee33_ccff), Color(0xee00_ff99)],
            angle_degrees: 45,
        })
    );
    assert_eq!(
        parse_gradient("rgb(ffffff)"),
        Ok(Gradient::solid(Color(0xffff_ffff)))
    );
    assert_eq!(
        parse_gradient("0xff000000 90deg ignored words"),
        Ok(Gradient {
            colors: vec![Color(0xff00_0000)],
            angle_degrees: 90,
        })
    );
    let ten = ["0xff000000"; 10].join(" ");
    assert!(parse_gradient(&ten).is_ok());
    assert_eq!(
        parse_gradient(&format!("{ten} 0xff000000")),
        Err("Too many colors in a gradient".to_owned())
    );
    assert_eq!(
        parse_gradient("45deg"),
        Err("Colors in gradient must be at least 1".to_owned())
    );
    assert!(parse_gradient("rgba(0,0,0,1) 1deg").is_ok());
    assert!(
        parse_gradient("rgba(0, 0, 0, 1)").is_err(),
        "spaces split a gradient's words, as in Hyprland"
    );
    assert!(parse_gradient("0xff000000 xdeg").is_err());
}

#[test]
fn gaps_spread_like_css() {
    let gaps = |top, right, bottom, left| Gaps {
        top,
        right,
        bottom,
        left,
    };
    assert_eq!(parse_gaps("5"), Ok(Gaps::all(5)));
    assert_eq!(parse_gaps("5 10"), Ok(gaps(5, 10, 5, 10)));
    assert_eq!(parse_gaps("1 2 3"), Ok(gaps(1, 2, 3, 2)));
    assert_eq!(parse_gaps("1 2 3 4"), Ok(gaps(1, 2, 3, 4)));
    assert!(parse_gaps("").is_err());
    assert!(parse_gaps("1 2 3 4 5").is_err());
    assert!(parse_gaps("1 x").is_err());
}

// -- Lines, comments, categories ----------------------------------------------------

#[test]
fn categories_nest_and_the_colon_shorthand_is_the_same_option() {
    let config = clean(
        "general {\n    border_size = 3\n    gaps_in = 2 4\n}\n\
         input {\n  repeat_rate = 50\n}\n\
         dwindle:pseudotile = true\n",
    );
    assert_eq!(config.int("general:border_size"), Some(3));
    assert_eq!(
        config.gaps("general:gaps_in"),
        Some(Gaps {
            top: 2,
            right: 4,
            bottom: 2,
            left: 4
        })
    );
    assert_eq!(config.int("input:repeat_rate"), Some(50));
    assert_eq!(config.bool("dwindle:pseudotile"), Some(true));
}

#[test]
fn a_later_line_overrides_an_earlier_one() {
    let config = clean("general:border_size = 2\ngeneral {\nborder_size = 4\n}\n");
    assert_eq!(config.int("general:border_size"), Some(4));
}

#[test]
fn comments_end_a_line_and_a_doubled_hash_is_a_hash() {
    let config = clean(
        "# a comment\n\
         general:layout = master # trailing\n\
         exec-once = echo a##b # not this\n\
         \t  # indented comment\n",
    );
    assert_eq!(config.str("general:layout"), Some("master"));
    assert_eq!(config.exec_once, ["echo a#b"]);
}

#[test]
fn a_line_that_begins_with_a_hash_is_a_comment_however_many_follow() {
    let config = clean(
        "################\n\
         ### MONITORS ###\n\
         #\n\
         \t## indented, doubled\n\
         general:layout = master\n",
    );
    assert_eq!(config.str("general:layout"), Some("master"));
}

#[test]
fn a_keyword_inside_a_category_is_the_keyword() {
    let config = clean(
        "animations {\n\
         enabled = true\n\
         bezier = quick, 0.15, 0, 0.1, 1\n\
         animation = fade, 1, 3.03, quick\n\
         }\n\
         general {\n\
         exec-once = bar\n\
         bind = SUPER, Q, exec, term\n\
         }\n",
    );
    let keywords: Vec<&str> = config
        .animations
        .iter()
        .map(|raw| raw.keyword.as_str())
        .collect();
    assert_eq!(keywords, ["bezier", "animation"]);
    assert_eq!(config.animations[1].value, "fade, 1, 3.03, quick");
    assert_eq!(config.bool("animations:enabled"), Some(true));
    assert_eq!(config.exec_once, ["bar"]);
    assert_eq!(config.binds.len(), 1);
}

#[test]
fn an_unknown_name_inside_a_category_is_still_the_category_s_option() {
    assert_eq!(
        messages("general {\nnot_a_thing = 1\n}\n"),
        [(
            2,
            "config option <general:not_a_thing> does not exist.".to_owned()
        )]
    );
}

#[test]
fn the_value_is_everything_after_the_first_equals_sign() {
    let config = clean("exec-once = env A=B C==D prog\n");
    assert_eq!(config.exec_once, ["env A=B C==D prog"]);
}

#[test]
fn errors_name_the_line_and_the_rest_of_the_file_still_applies() {
    let parsed = parse(
        "main.conf",
        "general:border_size = 2\n\
         general:nonexistent = 1\n\
         general:border_size = wide\n\
         nonsense\n\
         }\n\
         general:gaps_in = 7\n",
        &mut NoSources,
    );
    assert_eq!(parsed.config.int("general:border_size"), Some(2));
    assert_eq!(parsed.config.gaps("general:gaps_in"), Some(Gaps::all(7)));
    assert_eq!(
        parsed.diagnostics,
        [
            Diagnostic {
                file: "main.conf".to_owned(),
                line: 2,
                message: "config option <general:nonexistent> does not exist.".to_owned(),
            },
            Diagnostic {
                file: "main.conf".to_owned(),
                line: 3,
                message: "error setting value <wide> for field <general:border_size>: \
                          cannot parse \"wide\" as an int."
                    .to_owned(),
            },
            Diagnostic {
                file: "main.conf".to_owned(),
                line: 4,
                message: "invalid line: nonsense".to_owned(),
            },
            Diagnostic {
                file: "main.conf".to_owned(),
                line: 5,
                message: "unexpected } with no category open".to_owned(),
            },
        ]
    );
    assert_eq!(
        parsed
            .diagnostics
            .first()
            .map(ToString::to_string)
            .as_deref(),
        Some(
            "Config error in file main.conf at line 2: \
             config option <general:nonexistent> does not exist."
        )
    );
}

#[test]
fn an_unclosed_category_is_reported_at_the_last_line() {
    assert_eq!(
        messages("general {\nborder_size = 2\n"),
        [(2, "category general is not closed".to_owned())]
    );
}

#[test]
fn what_is_not_supported_yet_says_so() {
    assert_eq!(
        messages(
            "# hyprlang noerror true\n\
             device[my-mouse] {\n\
             general:border_size = {{ 1 + 1 }}\n"
        ),
        [
            (1, "hyprlang directives are not supported yet".to_owned()),
            (
                2,
                "keyed category device[my-mouse] is not supported yet".to_owned()
            ),
            (3, "hyprlang expressions are not supported yet".to_owned()),
        ]
    );
}

#[test]
fn crlf_line_endings_read_the_same() {
    let config = clean("general {\r\nborder_size = 3\r\n}\r\n");
    assert_eq!(config.int("general:border_size"), Some(3));
}

// -- Variables ---------------------------------------------------------------------

#[test]
fn variables_expand_in_values_longest_name_first() {
    let config = clean(
        "$main = ALT\n\
         $mainMod = SUPER\n\
         $terminal = kitty --class $main\n\
         bind = $mainMod, Q, exec, $terminal\n\
         bind = $main, W, exec, $unset and $5\n",
    );
    assert_eq!(
        config.variables.get("terminal").map(String::as_str),
        Some("kitty --class ALT"),
        "a definition expands the variables before it"
    );
    let [super_q, alt_w] = config.binds.as_slice() else {
        panic!("two bindings, not {:?}", config.binds);
    };
    assert_eq!(super_q.mods, Mods(Mods::LOGO));
    assert_eq!(super_q.arg, "kitty --class ALT");
    assert_eq!(alt_w.mods, Mods(Mods::ALT));
    assert_eq!(alt_w.arg, "$unset and $5", "an unknown $ stays as written");
}

#[test]
fn a_variable_redefined_applies_from_then_on() {
    let config = clean("$gap = 3\ngeneral:gaps_in = $gap\n$gap = 9\ngeneral:gaps_out = $gap\n");
    assert_eq!(config.gaps("general:gaps_in"), Some(Gaps::all(3)));
    assert_eq!(config.gaps("general:gaps_out"), Some(Gaps::all(9)));
}

#[test]
fn a_variable_doubling_itself_stops_at_the_value_limit() {
    // Twenty doublings of two bytes is two megabytes.
    let text = format!("$a = xx\n{}", "$a = $a$a\n".repeat(20));
    let parsed = parse("hyprland.conf", &text, &mut NoSources);
    let limit = format!("a value longer than {MAX_VALUE_LEN} bytes once variables are expanded");
    assert!(
        parsed.diagnostics.iter().any(|d| d.message == limit),
        "the value limit was reached: {:?}",
        parsed.diagnostics.first()
    );
    let longest = parsed.config.variables.get("a").map_or(0, String::len);
    assert!(longest <= MAX_VALUE_LEN, "$a grew to {longest} bytes");
}

#[test]
fn a_variable_needs_a_name() {
    assert_eq!(
        messages("$ = 1\n$a b = 2\n"),
        [
            (1, "invalid variable name $".to_owned()),
            (2, "invalid variable name $a b".to_owned()),
        ]
    );
}

// -- Bindings ----------------------------------------------------------------------

#[test]
fn modifiers_are_found_anywhere_in_the_text() {
    assert_eq!(Mods::parse("SUPER"), Mods(Mods::LOGO));
    assert_eq!(Mods::parse("super_shift"), Mods(Mods::LOGO | Mods::SHIFT));
    assert_eq!(Mods::parse("SUPERSHIFT"), Mods(Mods::LOGO | Mods::SHIFT));
    assert_eq!(
        Mods::parse("CONTROL ALT MOD2 MOD3 MOD5 CAPS"),
        Mods(Mods::CTRL | Mods::ALT | Mods::MOD2 | Mods::MOD3 | Mods::MOD5 | Mods::CAPS)
    );
    for logo in ["WIN", "LOGO", "MOD4", "META"] {
        assert_eq!(Mods::parse(logo), Mods(Mods::LOGO));
    }
    assert_eq!(Mods::parse("MOD1"), Mods(Mods::ALT));
    assert_eq!(Mods::parse(""), Mods(0));
}

#[test]
fn a_binding_has_mods_key_dispatcher_and_an_argument_with_commas() {
    let config = clean(
        "bind = SUPER SHIFT, Return, Exec, kitty -e sh -c 'a, b'\n\
         bind = , XF86AudioMute, exec, pamixer -t\n\
         bind = SUPER, Q, killactive,\n\
         bind = SUPER, F, fullscreen\n",
    );
    let bind = |mods, key: &str, dispatcher: &str, arg: &str| Bind {
        flags: BindFlags::default(),
        mods: Mods(mods),
        key: Key::Sym(key.to_owned()),
        description: String::new(),
        dispatcher: dispatcher.to_owned(),
        arg: arg.to_owned(),
        submap: None,
    };
    assert_eq!(
        config.binds,
        [
            bind(
                Mods::LOGO | Mods::SHIFT,
                "Return",
                "exec",
                "kitty -e sh -c 'a, b'"
            ),
            bind(0, "XF86AudioMute", "exec", "pamixer -t"),
            bind(Mods::LOGO, "Q", "killactive", ""),
            bind(Mods::LOGO, "F", "fullscreen", ""),
        ]
    );
}

#[test]
fn flags_codes_mouse_buttons_and_descriptions() {
    let config = clean(
        "bindel = , code:123, exec, volume up\n\
         bindm = SUPER, mouse:272, movewindow\n\
         bindd = SUPER, T, Open a terminal, exec, kitty\n\
         bind = SUPER, mouse_down, workspace, e+1\n",
    );
    let [volume, drag, described, wheel] = config.binds.as_slice() else {
        panic!("four bindings, not {:?}", config.binds);
    };
    assert_eq!(
        volume.flags,
        BindFlags {
            repeat: true,
            locked: true,
            ..BindFlags::default()
        }
    );
    assert_eq!(volume.key, Key::Code(123));
    assert_eq!(
        (drag.dispatcher.as_str(), drag.arg.as_str()),
        ("mouse", "movewindow")
    );
    assert_eq!(drag.key, Key::Mouse(272));
    assert!(drag.flags.mouse);
    assert_eq!(described.description, "Open a terminal");
    assert_eq!(
        (described.dispatcher.as_str(), described.arg.as_str()),
        ("exec", "kitty")
    );
    assert_eq!(wheel.key, Key::Wheel("mouse_down".to_owned()));
    assert_eq!(
        [volume, drag, described, wheel].map(|bind| bind.key.to_string()),
        ["code:123", "mouse:272", "T", "mouse_down"]
    );
}

#[test]
fn every_flag_letter() {
    let flags = BindFlags::parse("lroenmtisdpcgu").expect("every letter is a flag");
    assert_eq!(
        flags,
        BindFlags {
            locked: true,
            release: true,
            long_press: true,
            repeat: true,
            non_consuming: true,
            mouse: true,
            transparent: true,
            ignore_mods: true,
            separate: true,
            description: true,
            bypass: true,
            click: true,
            drag: true,
            submap_universal: true,
        }
    );
}

#[test]
fn bad_bindings() {
    assert_eq!(
        messages(
            "bindx = SUPER, Q, exec, a\n\
             bind = HYPER, Q, exec, a\n\
             bind = SUPER, code:abc, exec, a\n\
             bind = SUPER, , exec, a\n"
        ),
        [
            (1, "bind: invalid flag x".to_owned()),
            (2, "Invalid mod: HYPER".to_owned()),
            (3, "Invalid key: code:abc".to_owned()),
        ],
        "an empty key is accepted and binds nothing, as in Hyprland"
    );
}

#[test]
fn submaps_collect_the_bindings_after_them() {
    let config = clean(
        "bind = ALT, R, submap, resize\n\
         submap = resize\n\
         binde = , right, resizeactive, 10 0\n\
         bind = , escape, submap, reset\n\
         submap = reset\n\
         bind = SUPER, Q, killactive\n",
    );
    let submaps: Vec<_> = config
        .binds
        .iter()
        .map(|bind| bind.submap.as_deref())
        .collect();
    assert_eq!(submaps, [None, Some("resize"), Some("resize"), None]);
}

#[test]
fn unbind_removes_every_binding_of_that_mods_and_key() {
    let config = clean(
        "bind = SUPER, Q, killactive\n\
         bind = SUPER, Q, exec, other\n\
         bind = SUPER SHIFT, Q, exit\n\
         unbind = SUPER, Q\n",
    );
    let left: Vec<_> = config
        .binds
        .iter()
        .map(|bind| bind.dispatcher.as_str())
        .collect();
    assert_eq!(left, ["exit"]);
}

// -- Other keywords ----------------------------------------------------------------

#[test]
fn keywords_collect_in_order() {
    let config = clean(
        "monitor = ,preferred,auto,1\n\
         workspace = 1, monitor:Virtual-1\n\
         windowrule = float, match:class ^(pavucontrol)$\n\
         windowrule = opacity 0.9 0.8, match:class ^(kitty)$\n\
         layerrule = blur, waybar\n\
         bezier = ease, 0.05, 0.9, 0.1, 1.05\n\
         animation = windows, 1, 7, ease\n\
         exec-once = waybar\n\
         exec-once = pattern-client red\n\
         exec = notify reload\n\
         exec-shutdown = goodbye\n\
         env = XCURSOR_SIZE, 24\n\
         env = GREETING, hello, world\n",
    );
    let values = |raws: &[crate::Raw]| -> Vec<(String, String)> {
        raws.iter()
            .map(|raw| (raw.keyword.clone(), raw.value.clone()))
            .collect()
    };
    let owned = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|&(a, b)| (a.to_owned(), b.to_owned()))
            .collect()
    };
    assert_eq!(
        values(&config.monitors),
        owned(&[("monitor", ",preferred,auto,1")])
    );
    assert_eq!(
        values(&config.workspaces),
        owned(&[("workspace", "1, monitor:Virtual-1")])
    );
    assert_eq!(
        values(&config.window_rules),
        owned(&[
            ("windowrule", "float, match:class ^(pavucontrol)$"),
            ("windowrule", "opacity 0.9 0.8, match:class ^(kitty)$"),
        ])
    );
    assert_eq!(
        values(&config.layer_rules),
        owned(&[("layerrule", "blur, waybar")])
    );
    assert_eq!(config.animations.len(), 2);
    assert_eq!(config.exec_once, ["waybar", "pattern-client red"]);
    assert_eq!(config.exec, ["notify reload"]);
    assert_eq!(config.exec_shutdown, ["goodbye"]);
    assert_eq!(
        config.env,
        owned(&[("XCURSOR_SIZE", "24"), ("GREETING", "hello, world")])
    );
    assert_eq!(
        messages("env = LONELY\n"),
        [(1, "env expects NAME, value, not \"LONELY\"".to_owned())]
    );
}

#[test]
fn hyprctl_keyword_applies_like_a_line() {
    let mut config = Config::default();
    assert_eq!(config.keyword("general:border_size", " 5 "), Ok(()));
    assert_eq!(config.int("general:border_size"), Some(5));
    assert_eq!(config.keyword("bind", "SUPER, Q, killactive"), Ok(()));
    assert_eq!(config.binds.len(), 1);
    assert!(config.keyword("general:nope", "1").is_err());
    assert!(config.keyword("source", "other.conf").is_err());
    assert!(config.keyword("submap", "resize").is_err());
}

// -- source ------------------------------------------------------------------------

#[test]
fn source_reads_in_place_with_its_own_line_numbers() {
    let mut sources = Memory::default();
    let _ = sources.0.insert(
        "colors.conf".to_owned(),
        "$accent = rgb(ff0000)\nbogus = 1\n".to_owned(),
    );
    let parsed = parse(
        "hyprland.conf",
        "general:border_size = 2\n\
         source = colors.conf\n\
         general:col.active_border = $accent\n\
         source = missing.conf\n",
        &mut sources,
    );
    assert_eq!(
        parsed.config.gradient("general:col.active_border"),
        Some(&Gradient::solid(Color(0xffff_0000)))
    );
    let places: Vec<_> = parsed
        .diagnostics
        .iter()
        .map(|d| (d.file.as_str(), d.line, d.message.as_str()))
        .collect();
    assert_eq!(
        places,
        [
            ("colors.conf", 2, "config option <bogus> does not exist."),
            ("hyprland.conf", 4, "source file missing.conf not found"),
        ]
    );
}

#[test]
fn a_file_sourcing_itself_stops() {
    let mut sources = Memory::default();
    let _ = sources
        .0
        .insert("loop.conf".to_owned(), "source = loop.conf\n".to_owned());
    let parsed = parse("loop.conf", "source = loop.conf\n", &mut sources);
    assert_eq!(parsed.diagnostics.len(), 1);
    assert_eq!(
        parsed.diagnostics.first().map(|d| d.message.as_str()),
        Some(
            format!("source file loop.conf: nested deeper than {MAX_SOURCE_DEPTH} files").as_str()
        )
    );
}

#[test]
fn a_file_sourcing_itself_twice_stops_at_the_file_limit() {
    let text = "source = loop.conf
source = loop.conf
";
    let mut sources = Memory::default();
    let _ = sources.0.insert("loop.conf".to_owned(), text.to_owned());
    let parsed = parse("loop.conf", text, &mut sources);
    let limit = format!("source file loop.conf: more than {MAX_SOURCED_FILES} files sourced");
    assert!(
        parsed.diagnostics.iter().any(|d| d.message == limit),
        "the file limit was reached"
    );
    assert!(
        parsed.diagnostics.len() <= 2 * MAX_SOURCED_FILES + 2,
        "{} diagnostics",
        parsed.diagnostics.len()
    );
}

#[test]
fn file_system_sources_resolve_relative_and_home_paths() {
    let root = std::env::temp_dir().join(format!("compositor-config-{}", std::process::id()));
    let nested = root.join("hypr");
    std::fs::create_dir_all(&nested).expect("a scratch directory");
    std::fs::write(nested.join("part.conf"), "general:border_size = 9\n").expect("write");
    std::fs::write(root.join("home.conf"), "general:layout = master\n").expect("write");

    let main = nested.join("hyprland.conf");
    let mut sources = FsSources {
        home: Some(root.clone()),
    };
    let parsed = parse(
        &main.display().to_string(),
        "source = part.conf\nsource = ~/home.conf\nsource = *.conf\n",
        &mut sources,
    );
    std::fs::remove_dir_all(&root).expect("clean up");

    assert_eq!(parsed.config.int("general:border_size"), Some(9));
    assert_eq!(parsed.config.str("general:layout"), Some("master"));
    let messages: Vec<_> = parsed
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(
        messages,
        ["source file *.conf: globs are not supported yet"]
    );
    assert!(
        FsSources { home: None }
            .resolve("~/x.conf", "a.conf")
            .is_err()
    );
}

// -- A whole file ------------------------------------------------------------------

/// A configuration in the shape of Hyprland's example, cut to what the
/// stage 18 exit test uses.
const EXAMPLE: &str = r"
# Stage 18's exit test configuration.
$mainMod = SUPER
$pattern = pattern-client

monitor = , preferred, auto, 1

exec-once = $pattern --color rgb(ff0000)
exec-once = $pattern --color rgb(0000ff)

general {
    gaps_in = 5
    gaps_out = 20
    border_size = 2
    col.active_border = rgba(33ccffee) rgba(00ff99ee) 45deg
    col.inactive_border = rgba(595959aa)
    layout = dwindle
}

decoration {
    rounding = 10
}

dwindle {
    pseudotile = true
    preserve_split = true
}

master {
    new_status = master
}

input {
    kb_layout = us
    follow_mouse = 1
}

bind = $mainMod, Q, exec, $pattern
bind = $mainMod, C, killactive,
bind = $mainMod, V, togglefloating,
bind = $mainMod, left, movefocus, l
bind = $mainMod SHIFT, left, movewindow, l
bind = $mainMod, 1, workspace, 1
bind = $mainMod SHIFT, 1, movetoworkspace, 1
bindm = $mainMod, mouse:272, movewindow

windowrule = float, match:class ^(pattern-float)$
";

#[test]
fn the_example_configuration() {
    let config = clean(EXAMPLE);
    assert_eq!(
        config.exec_once,
        [
            "pattern-client --color rgb(ff0000)",
            "pattern-client --color rgb(0000ff)"
        ]
    );
    assert_eq!(config.int("general:border_size"), Some(2));
    assert_eq!(
        config.option("general:col.inactive_border"),
        Some(&OptionValue::Gradient(Gradient::solid(Color(0xaa59_5959))))
    );
    assert_eq!(config.int("decoration:rounding"), Some(10));
    assert_eq!(config.bool("dwindle:preserve_split"), Some(true));
    assert_eq!(config.str("master:new_status"), Some("master"));
    assert_eq!(config.binds.len(), 8);
    let dispatchers: Vec<_> = config
        .binds
        .iter()
        .map(|bind| (bind.dispatcher.as_str(), bind.arg.as_str()))
        .collect();
    assert_eq!(
        dispatchers,
        [
            ("exec", "pattern-client"),
            ("killactive", ""),
            ("togglefloating", ""),
            ("movefocus", "l"),
            ("movewindow", "l"),
            ("workspace", "1"),
            ("movetoworkspace", "1"),
            ("mouse", "movewindow"),
        ]
    );
    assert_eq!(config.window_rules.len(), 1);
}

// ---------------------------------------------------------------------------
// `monitor =` lines
//
// The form is Hyprland's `ConfigManager::handleMonitor`: a name, a
// resolution, a position and a scale, with an empty name standing for every
// monitor no other rule names.
// ---------------------------------------------------------------------------

use crate::{AddedReserved, Mode, MonitorRule, Position, Scale, Transform};

#[test]
fn a_monitor_line_is_a_name_a_mode_a_place_and_a_scale() {
    let rule = MonitorRule::parse("Virtual-1, 1920x1080@60, 0x0, 2").unwrap();
    assert_eq!(rule.name, "Virtual-1");
    assert!(!rule.disabled);
    assert_eq!(
        rule.mode,
        Mode::Fixed {
            width: 1920,
            height: 1080,
            refresh: Some(60.0),
        }
    );
    assert_eq!(rule.position, Position::At(0, 0));
    assert_eq!(rule.scale, Scale::Fixed(2.0));
    assert!((rule.scale_factor() - 2.0).abs() < f64::EPSILON);

    // The line every example configuration carries: no name, so it is the
    // rule for whatever monitor there is.
    let any = MonitorRule::parse(", preferred, auto, 1").unwrap();
    assert!(any.name.is_empty());
    assert!(any.matches("Virtual-1", ""));
    assert!(any.matches("DP-3", "Dell Inc. DELL P2418D MY3ND91J09CT"));
    assert_eq!(any.mode, Mode::Preferred);
    assert_eq!(any.position, Position::Auto);

    // A named rule is that monitor's and no other's.
    assert!(rule.matches("Virtual-1", ""));
    assert!(!rule.matches("Virtual-2", ""));

    // `desc:` matches the start of what the monitor says it is, which is
    // how a real configuration names a screen: a connector's name moves
    // when a cable does and a description does not. The prefix matters --
    // the make and the model name every one of that model on the machine,
    // and the serial after them names one.
    let described =
        MonitorRule::parse("desc:Dell Inc. DELL P2418D MY3ND91J09CT, preferred, 0x0, 1").unwrap();
    assert!(described.matches("DP-3", "Dell Inc. DELL P2418D MY3ND91J09CT"));
    assert!(!described.matches("DP-3", "Dell Inc. DELL P2418D XXXXXXXXXXX"));
    assert!(
        !described.matches("desc:Dell Inc. DELL P2418D MY3ND91J09CT", ""),
        "a `desc:` rule is not a connector called that"
    );
    let model = MonitorRule::parse("desc:Dell Inc. DELL P2418D, preferred, auto, 1").unwrap();
    assert!(
        model.matches("DP-3", "Dell Inc. DELL P2418D MY3ND91J09CT"),
        "the make and the model name every one of that model"
    );
    assert!(!model.matches("DP-3", "Lenovo Group Limited R27qe Gen2 UTP03KBB"));

    // A position without a refresh rate, and `auto` for the scale.
    let placed = MonitorRule::parse("DP-1, 2560x1440, 1920x0, auto").unwrap();
    assert_eq!(placed.position, Position::At(1920, 0));
    assert_eq!(placed.scale, Scale::Auto);
    assert!((placed.scale_factor() - 1.0).abs() < f64::EPSILON);

    // `highres` and `highrr` choose between the modes a connector lists
    // (`docs/DISPLAY.md` §9.5); `maxwidth` is Hyprland's preferred.
    for (word, mode) in [
        ("preferred", Mode::Preferred),
        ("maxwidth", Mode::Preferred),
        ("highres", Mode::HighRes),
        ("highrr", Mode::HighRr),
    ] {
        assert_eq!(
            MonitorRule::parse(&format!("Virtual-1, {word}, auto, 1"))
                .unwrap()
                .mode,
            mode
        );
    }
}

#[test]
fn a_monitor_can_be_disabled_and_a_bad_field_is_refused() {
    let off = MonitorRule::parse("Virtual-2, disable").unwrap();
    assert!(off.disabled);
    assert_eq!(off.name, "Virtual-2");

    for bad in [
        "Virtual-1, 1920, auto, 1",
        "Virtual-1, 1920x1080@sixty, auto, 1",
        "Virtual-1, preferred, over-there, 1",
        "Virtual-1, preferred, auto, 0",
        "Virtual-1, preferred, auto, -2",
        "Virtual-1, preferred, auto, half",
    ] {
        assert!(MonitorRule::parse(bad).is_err(), "{bad} was read");
    }

    // What is not done says so rather than being ignored.
    let mirror = MonitorRule::parse("Virtual-1, preferred, auto, 1, mirror, DP-1");
    assert!(
        mirror
            .as_ref()
            .is_err_and(|why| why.contains("not done yet")),
        "{mirror:?}"
    );
    let vrr = MonitorRule::parse("Virtual-1, preferred, auto, 1, transform, 1, vrr, 1");
    assert!(
        vrr.as_ref().is_err_and(|why| why.contains("not done yet")),
        "{vrr:?}"
    );
    // And a field that is no keyword at all is Hyprland's syntax error.
    assert_eq!(
        MonitorRule::parse("Virtual-1, preferred, auto, 1, 1"),
        Err("invalid syntax at \"1\"".to_owned())
    );
    let auto_left = MonitorRule::parse("Virtual-1, preferred, auto-left, 1");
    assert!(
        auto_left
            .as_ref()
            .is_err_and(|why| why.contains("not done yet")),
        "{auto_left:?}"
    );
}

/// `transform, N` after the scale: Hyprland's `wl_output.transform` number,
/// 0 to 7, as `CMonitorRuleParser::parseTransform` reads it.
#[test]
fn a_monitor_can_be_turned() {
    let turned = MonitorRule::parse("HDMI-A-1, 1920x1080@60, 0x0, 1, transform, 1").unwrap();
    assert_eq!(turned.transform, Transform::Rotated90);
    assert_eq!(
        turned.mode,
        Mode::Fixed {
            width: 1920,
            height: 1080,
            refresh: Some(60.0)
        },
        "the mode is the connector's, not turned"
    );
    // A line that does not say is upright.
    assert_eq!(
        MonitorRule::parse(", preferred, auto, 1")
            .unwrap()
            .transform,
        Transform::Normal
    );
    // Every number the protocol has.
    for value in 0..8u32 {
        let rule =
            MonitorRule::parse(&format!("DP-1, preferred, auto, 1, transform, {value}")).unwrap();
        assert_eq!(rule.transform.value(), value);
    }
    // Hyprland reads it as a whole number and nothing else.
    for bad in ["8", "-1", "1.0", "ninety", ""] {
        let line = format!("DP-1, preferred, auto, 1, transform, {bad}");
        assert!(
            MonitorRule::parse(&line).is_err_and(|why| why.starts_with("invalid transform")),
            "{line} was read"
        );
    }
    // A pair with no value is not one.
    assert!(MonitorRule::parse("DP-1, preferred, auto, 1, transform").is_err());
    // A later pair of the same keyword wins, as Hyprland's loop has it.
    assert_eq!(
        MonitorRule::parse("DP-1, preferred, auto, 1, transform, 1, transform, 3")
            .unwrap()
            .transform,
        Transform::Rotated270
    );
    // Scale and transform together: the transform does not change the
    // scale it is read beside.
    let both = MonitorRule::parse("DP-1, preferred, auto, 2, transform, 3").unwrap();
    assert!((both.scale_factor() - 2.0).abs() < f64::EPSILON);
    assert_eq!(both.transform, Transform::Rotated270);
}

/// `monitor = NAME, transform, N` on its own line turns the monitor an
/// earlier line named, and names no monitor of its own.
#[test]
fn a_transform_line_turns_an_earlier_rule() {
    let (rules, refused) = MonitorRule::read_all([
        "DP-1, 2560x1440, 0x0, 1",
        "HDMI-A-1, preferred, auto, 1",
        "DP-1, transform, 3",
        // No earlier rule is called this: nothing happens, and it is no
        // error.
        "DP-9, transform, 1",
        "HDMI-A-1, transform, 9",
    ]);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].0, "HDMI-A-1, transform, 9");
    assert_eq!(rules.len(), 3, "{rules:?}");
    // The last rule for DP-1 is the turned copy, and it is the one that
    // applies, since a later rule wins.
    let last = rules
        .iter()
        .rev()
        .find(|rule| rule.matches("DP-1", ""))
        .unwrap();
    assert_eq!(last.transform, Transform::Rotated270);
    assert_eq!(last.position, Position::At(0, 0));
    assert!(matches!(
        last.mode,
        Mode::Fixed {
            width: 2560,
            height: 1440,
            ..
        }
    ));
    // Read as a rule of its own, the short form says what it is.
    assert!(MonitorRule::parse("DP-1, transform, 3").is_err());
}

/// `monitor = NAME, addreserved, TOP, BOTTOM, LEFT, RIGHT` is no rule of
/// its own: it keeps strips of the monitor free, the last line for a
/// monitor wins, and zeros give them back -- which is how the Pixel 7's VM
/// follows Android's keyboard up and down.
#[test]
fn addreserved_keeps_strips_free_and_the_last_line_wins() {
    let lines = [
        ", 1080x2400, auto, 2",
        "Virtual-1, addreserved, 0, 400, 0, 0",
        "DP-1, addreserved, 30, 0, 0, 0",
        "Virtual-1, addreserved, 0, 350, 0, 0",
        "Virtual-1, addreserved, 0, -1, 0, 0",
        "Virtual-1, addreserved, 0, 1",
    ];
    let (rules, refused) = MonitorRule::read_all(lines);
    assert_eq!(rules.len(), 1, "only the first line is a rule: {rules:?}");
    assert_eq!(
        refused.iter().map(|(line, _)| *line).collect::<Vec<_>>(),
        [
            "Virtual-1, addreserved, 0, -1, 0, 0",
            "Virtual-1, addreserved, 0, 1"
        ],
        "a negative strip and a short line are said"
    );

    let added = AddedReserved::read_all(lines);
    assert_eq!(added.len(), 3, "{added:?}");
    assert_eq!(
        AddedReserved::for_monitor(&added, "Virtual-1", ""),
        Gaps {
            top: 0,
            right: 0,
            bottom: 350,
            left: 0
        },
        "the later line"
    );
    assert_eq!(AddedReserved::for_monitor(&added, "DP-1", "").top, 30);
    assert_eq!(
        AddedReserved::for_monitor(&added, "HDMI-A-1", ""),
        Gaps::all(0),
        "a monitor no line names"
    );

    let back = AddedReserved::read_all(lines.into_iter().chain(["Virtual-1,addreserved,0,0,0,0"]));
    assert_eq!(
        AddedReserved::for_monitor(&back, "Virtual-1", ""),
        Gaps::all(0),
        "hyprctl's form, without spaces, gives them back"
    );
}

// ---------------------------------------------------------------------------
// `windowrule =` lines
//
// Hyprland 0.56's form: comma-separated fields, each a name and a value, with
// `match:` in front of the ones the window must be.
// ---------------------------------------------------------------------------

use crate::{Decoration, Effect, Length, Window, WindowRule};

fn window<'a>(class: &'a str, title: &'a str) -> Window<'a> {
    Window {
        class,
        title,
        initial_class: class,
        initial_title: title,
        ..Window::default()
    }
}

#[test]
fn a_rule_is_what_to_do_and_what_to_do_it_to() {
    let rule = WindowRule::parse("float, match:class ^(foot)$").unwrap();
    assert_eq!(rule.effects, [Effect::Float]);
    assert!(rule.matches(&window("foot", "a shell")));
    assert!(!rule.matches(&window("kitty", "a shell")));

    // Several effects and several matchers, all of which must hold.
    let rule =
        WindowRule::parse("float, size 800 600, match:class ^(foot)$, match:title ^(Save.*)$")
            .unwrap();
    assert_eq!(
        rule.effects,
        [
            Effect::Float,
            Effect::Size(Length::Pixels(800), Length::Pixels(600))
        ]
    );
    assert!(rule.matches(&window("foot", "Save as")));
    assert!(!rule.matches(&window("foot", "a shell")));
    assert!(!rule.matches(&window("kitty", "Save as")));

    // A rule with no `match:` applies to every window, which is what a line
    // with none means.
    let every = WindowRule::parse("no_shadow").unwrap();
    assert_eq!(every.effects, [Effect::Without(Decoration::Shadow)]);
    assert!(every.matches(&window("anything", "at all")));
}

#[test]
fn the_effects_take_their_values() {
    let effects = |line: &str| WindowRule::parse(line).unwrap().effects;
    assert_eq!(effects("tile"), [Effect::Tile]);
    assert_eq!(effects("center"), [Effect::Center]);
    assert_eq!(effects("move center"), [Effect::Center]);
    assert_eq!(
        effects("move 100 200"),
        [Effect::Move(Length::Pixels(100), Length::Pixels(200))]
    );
    // A percentage is of the monitor.
    assert_eq!(
        effects("size 50% 100%"),
        [Effect::Size(Length::Share(0.5), Length::Share(1.0))]
    );
    assert_eq!(Length::Share(0.5).against(1024), 512);
    assert_eq!(Length::Pixels(640).against(1024), 640);
    assert_eq!(
        effects("workspace 3"),
        [Effect::Workspace {
            target: "3".to_owned(),
            silent: false
        }]
    );
    assert_eq!(
        effects("workspace 3 silent"),
        [Effect::Workspace {
            target: "3".to_owned(),
            silent: true
        }]
    );
    assert_eq!(effects("opacity 0.8"), [Effect::Opacity(0.8)]);
    // Hyprland's second number is the unfocused opacity; the first is what
    // this carries.
    assert_eq!(effects("opacity 0.8 0.6"), [Effect::Opacity(0.8)]);
    assert_eq!(effects("rounding 12"), [Effect::Rounding(12)]);
    assert_eq!(effects("border_size 3"), [Effect::BorderSize(3)]);
    assert_eq!(effects("fullscreen"), [Effect::Fullscreen]);
    assert_eq!(effects("no_focus"), [Effect::NoFocus]);
    assert_eq!(effects("no_blur"), [Effect::Without(Decoration::Blur)]);
    assert_eq!(effects("no_dim"), [Effect::Without(Decoration::Dim)]);
}

#[test]
fn the_states_a_rule_can_match_on() {
    let floating = WindowRule::parse("no_shadow, match:float 1").unwrap();
    let tiled = Window {
        floating: false,
        ..window("foot", "a shell")
    };
    assert!(!floating.matches(&tiled));
    assert!(floating.matches(&Window {
        floating: true,
        ..tiled
    }));

    let unfocused = WindowRule::parse("opacity 0.7, match:focus 0").unwrap();
    assert!(unfocused.matches(&tiled));
    assert!(!unfocused.matches(&Window {
        focused: true,
        ..tiled
    }));

    // The initial names are matched apart from the current ones.
    let initial = WindowRule::parse("float, match:initial_title ^(one)$").unwrap();
    assert!(initial.matches(&Window {
        title: "renamed",
        initial_title: "one",
        ..window("pattern", "one")
    }));
    assert!(!initial.matches(&window("pattern", "one what")));
}

/// The matchers and the effects the merged 0.56 grammar added, which a
/// real configuration writes.
///
/// `example`'s two `windowrule` lines are `suppress_event maximize,
/// match:class .*` and a `no_focus` line matching on `xwayland`, `pin` and
/// `float` at once. Before this both were refused outright, and a refused
/// line is a line that does nothing: the person who wrote it gets a
/// diagnostic and the window they meant to keep unfocused takes the focus.
#[test]
fn the_matchers_and_effects_of_the_merged_grammar() {
    let effects = |line: &str| WindowRule::parse(line).map(|rule| rule.effects);

    assert_eq!(
        effects("suppress_event maximize"),
        Ok(vec![Effect::Suppress(vec!["maximize".to_owned()])])
    );
    assert_eq!(
        effects("suppress_event maximize fullscreen"),
        Ok(vec![Effect::Suppress(vec![
            "maximize".to_owned(),
            "fullscreen".to_owned()
        ])])
    );
    assert_eq!(effects("pin"), Ok(vec![Effect::Pin]));
    assert_eq!(effects("pseudo"), Ok(vec![Effect::Pseudo]));
    assert_eq!(
        effects("tag music"),
        Ok(vec![Effect::Tag("music".to_owned())])
    );
    // An effect Hyprland has that this compositor does not carry out is
    // kept by name, so that the matchers written beside it still apply.
    assert_eq!(
        effects("no_shortcuts_inhibit true, float"),
        Ok(vec![
            Effect::Unhandled("no_shortcuts_inhibit".to_owned()),
            Effect::Float
        ])
    );
    // A word that is not in Hyprland's table at all is still a typo.
    assert!(effects("levitate").is_err());

    let base = window("foot", "a shell");
    let pinned = WindowRule::parse("float, match:pin 1").unwrap();
    assert!(!pinned.matches(&base));
    assert!(pinned.matches(&Window {
        pinned: true,
        ..base
    }));

    let modal = WindowRule::parse("float, match:modal 1").unwrap();
    assert!(modal.matches(&Window {
        modal: true,
        ..base
    }));

    let grouped = WindowRule::parse("float, match:group 1").unwrap();
    assert!(grouped.matches(&Window {
        grouped: true,
        ..base
    }));

    // There is no XWayland here, so a rule that asks for an X11 window
    // matches nothing and one that asks for a native window matches
    // everything. `example`'s `no_focus` line asks for `xwayland 1`, and
    // that line is meant not to fire.
    assert!(
        !WindowRule::parse("no_focus, match:xwayland 1")
            .unwrap()
            .matches(&base)
    );
    assert!(
        WindowRule::parse("no_focus, match:xwayland 0")
            .unwrap()
            .matches(&base)
    );

    let tagged = WindowRule::parse("opacity 0.5, match:tag ^(music)$").unwrap();
    let tags = ["music".to_owned()];
    assert!(!tagged.matches(&base));
    assert!(tagged.matches(&Window {
        tags: &tags,
        ..base
    }));

    let on = WindowRule::parse("float, match:workspace ^(3)$").unwrap();
    assert!(on.matches(&Window {
        workspace: "3",
        ..base
    }));
    assert!(!on.matches(&Window {
        workspace: "4",
        ..base
    }));

    let named = WindowRule::parse("float, match:xdg_tag ^(main)$").unwrap();
    assert!(named.matches(&Window {
        xdg_tag: "main",
        ..base
    }));

    let playing = WindowRule::parse("immediate true, match:content ^(game)$").unwrap();
    assert!(playing.matches(&Window {
        content: "game",
        ..base
    }));
    assert!(!playing.matches(&base));

    let state = WindowRule::parse("float, match:fullscreen_state_internal 1").unwrap();
    assert!(state.matches(&Window {
        fullscreen_state_internal: 1,
        ..base
    }));
    assert!(!state.matches(&base));

    // And the whole of the line `example` writes, which must read and must
    // not fire.
    let real = WindowRule::parse(
        "no_focus true, match:class ^$, match:title ^$, match:xwayland 1, match:float 1, \
         match:fullscreen 0, match:pin 0",
    )
    .expect("the line a real configuration writes");
    assert!(!real.matches(&Window {
        class: "",
        title: "",
        floating: true,
        ..base
    }));
}

/// The four effects that reach the renderer through `WindowStyle`.
///
/// `rounding_power` makes the corner a superellipse, `border_color` gives
/// one window its own border, `decorate` takes the border and the shadow
/// away from a window that draws its own frame, and `opaque` says a
/// client's alpha channel is not to be believed.
#[test]
fn the_effects_that_change_how_one_window_is_drawn() {
    let effects = |line: &str| WindowRule::parse(line).map(|rule| rule.effects);

    assert_eq!(
        effects("rounding_power 4"),
        Ok(vec![Effect::RoundingPower(4.0)])
    );
    // Hyprland clamps it to one and ten.
    assert_eq!(
        effects("rounding_power 99"),
        Ok(vec![Effect::RoundingPower(10.0)])
    );
    assert!(effects("rounding_power round").is_err());

    assert_eq!(
        effects("border_color rgba(ff0000ff)").map(|held| held
            .iter()
            .map(|effect| match effect {
                Effect::BorderColor(gradient) => gradient.colors.clone(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>()),
        Ok(vec![vec![Color(0xFFFF_0000)]])
    );
    // Two colours and an angle, which a gradient border is.
    assert_eq!(
        effects("border_color rgba(33ccffee) rgba(00ff99ee) 45deg").map(|held| held.len()),
        Ok(1)
    );

    // A bare word is true, which is Hyprland's `truthy`.
    assert_eq!(effects("decorate"), Ok(vec![Effect::Decorate(true)]));
    assert_eq!(effects("decorate false"), Ok(vec![Effect::Decorate(false)]));
    assert_eq!(effects("opaque"), Ok(vec![Effect::Opaque(true)]));
    assert_eq!(effects("opaque 0"), Ok(vec![Effect::Opaque(false)]));
}

#[test]
fn a_rule_that_cannot_be_read_says_what_is_wrong() {
    for bad in [
        // A field with no value.
        "match:class",
        // A matcher nothing has.
        "float, match:colour blue",
        // An effect nothing does.
        "levitate",
        // A value of the wrong shape.
        "size 800",
        "size wide high",
        "opacity green",
        "opacity 2",
        "rounding round",
        "workspace",
        // A pattern the engine will not take.
        r"float, match:class ^(\d+)$",
        // Nothing to do at all.
        "match:class ^(foot)$",
    ] {
        assert!(WindowRule::parse(bad).is_err(), "`{bad}` was read");
    }
}

/// `windowrulev2` is refused with Hyprland's own words: 0.56 merged the two
/// syntaxes and took the old one away.
#[test]
fn windowrulev2_is_refused_as_hyprland_refuses_it() {
    let parsed = parse(
        "t.conf",
        "windowrulev2 = float,class:^(foot)$\nwindowrule = float, match:class ^(foot)$\n",
        &mut NoSources,
    );
    assert_eq!(parsed.diagnostics.len(), 1, "{:?}", parsed.diagnostics);
    assert!(
        parsed.diagnostics[0].message.contains("deprecated"),
        "{:?}",
        parsed.diagnostics[0]
    );
    // And the line that is not deprecated is kept.
    assert_eq!(parsed.config.window_rules.len(), 1);
}

// -- `layerrule` --------------------------------------------------------------

/// Each rule is read, and a line that is not one says so rather than
/// costing a person the rest of their configuration.
///
/// Hyprland 0.56 reads a `layerrule` with the same grammar as a
/// `windowrule`: fields separated by commas, each a name and a value, and
/// the namespace under `match:namespace`. The old `blur, waybar` form is
/// gone, and a compositor that still took it would quietly accept a line
/// Hyprland refuses.
#[test]
fn a_layerrule_is_read_or_refused_with_a_reason() {
    use crate::{LayerEffect, LayerRule};

    let rule = LayerRule::parse("blur true, match:namespace waybar").expect("a rule");
    assert_eq!(rule.effects, vec![LayerEffect::Blur(true)]);
    assert!(rule.matches("waybar"));
    assert!(!rule.matches("swaync"));

    assert_eq!(
        LayerRule::parse("ignore_alpha 0.5, match:namespace waybar").map(|rule| rule.effects),
        Ok(vec![LayerEffect::IgnoreAlpha(0.5)])
    );
    assert_eq!(
        LayerRule::parse("order 10, match:namespace notifications").map(|rule| rule.effects),
        Ok(vec![LayerEffect::Order(10)])
    );
    // One line may carry several effects, which is what the merged
    // grammar bought.
    assert_eq!(
        LayerRule::parse("blur true, ignore_alpha 0.2, match:namespace waybar")
            .map(|rule| rule.effects),
        Ok(vec![LayerEffect::Blur(true), LayerEffect::IgnoreAlpha(0.2)])
    );
    // Hyprland's `truthy`: `1`, or a word beginning true, yes or on.
    assert_eq!(
        LayerRule::parse("no_anim yes, match:namespace bar").map(|rule| rule.effects),
        Ok(vec![LayerEffect::NoAnim(true)])
    );
    assert_eq!(
        LayerRule::parse("no_anim 0, match:namespace bar").map(|rule| rule.effects),
        Ok(vec![LayerEffect::NoAnim(false)])
    );
    // `above_lock` is a level, clamped rather than refused.
    assert_eq!(
        LayerRule::parse("above_lock 2, match:namespace keyboard").map(|rule| rule.effects),
        Ok(vec![LayerEffect::AboveLock(2)])
    );
    assert_eq!(
        LayerRule::parse("above_lock 7, match:namespace keyboard").map(|rule| rule.effects),
        Ok(vec![LayerEffect::AboveLock(2)])
    );
    // A rule with no namespace applies everywhere.
    let every = LayerRule::parse("blur true").expect("a rule");
    assert!(every.matches("anything"));
    // A property a layer surface does not have is accepted and skipped,
    // as Hyprland's own engine does with it.
    assert!(LayerRule::parse("blur true, match:class foot").is_ok());

    assert!(LayerRule::parse("blur").is_err(), "no value");
    assert!(
        LayerRule::parse("wobble true, match:namespace waybar").is_err(),
        "no such rule"
    );
    assert!(
        LayerRule::parse("order x, match:namespace waybar").is_err(),
        "not a number"
    );
    assert!(
        LayerRule::parse("match:namespace waybar").is_err(),
        "nothing to do"
    );
}

/// Every rule that matches one surface is applied, later lines winning.
#[test]
fn the_rules_that_match_a_namespace_are_gathered() {
    use crate::{LayerRule, Layered};

    let rules: Vec<LayerRule> = [
        "blur true, match:namespace ^(waybar)$",
        "ignore_alpha 0.3, match:namespace waybar",
        "order 2, match:namespace waybar",
        "order 5, match:namespace waybar",
        "above_lock 1, match:namespace keyboard",
    ]
    .iter()
    .map(|line| LayerRule::parse(line).expect("a rule"))
    .collect();

    let bar = Layered::of(&rules, "waybar");
    assert!(bar.blur);
    assert_eq!(bar.ignore_alpha, Some(0.3));
    assert_eq!(bar.order, 5, "the later line wins");
    assert!(!bar.above_lock);
    assert!(bar.animates, "nothing turned it off");

    let keyboard = Layered::of(&rules, "keyboard");
    assert!(keyboard.above_lock);
    assert!(!keyboard.blur);

    // A namespace nothing names gets the defaults.
    assert_eq!(
        Layered::of(&rules, "nothing"),
        Layered {
            animates: true,
            ..Layered::default()
        }
    );
}

// -- `workspace` rules --------------------------------------------------------

/// A `workspace =` line is read into what it changes.
///
/// Hyprland calls the keyword `workspace` and the thing it makes a
/// workspace rule; the fields are `key:value` and the first one names the
/// workspace. Three of them are stored as their negations, which is how
/// `hyprctl workspacerules` prints them, and a field the line did not set
/// is *unset* rather than false: `border:` unset leaves
/// `general:border_size` alone and `border:true` insists on it.
#[test]
fn a_workspace_rule_is_read() {
    use crate::{Which, WorkspaceRule};

    let rule = WorkspaceRule::parse("1, monitor:DP-1, default:true").expect("a rule");
    assert_eq!(rule.which, Which::Id(1));
    assert_eq!(rule.workspace, "1");
    assert_eq!(rule.monitor.as_deref(), Some("DP-1"));
    assert_eq!(rule.is_default, Some(true));
    assert_eq!(rule.is_persistent, None, "the line did not say");

    let rule = WorkspaceRule::parse(
        "name:code, gapsin:0, gapsout:5 10, bordersize:3, border:false, shadow:0, rounding:0, \
         decorate:true, layout:master, layoutopt:orientation:top, defaultName:code, \
         persistent:1, on-created-empty:foot, animation:slide",
    )
    .expect("a rule");
    assert_eq!(rule.which, Which::Name("code".to_owned()));
    assert_eq!(rule.gaps_in, Some(Gaps::all(0)));
    assert_eq!(
        rule.gaps_out,
        Some(Gaps {
            top: 5,
            right: 10,
            bottom: 5,
            left: 10
        })
    );
    assert_eq!(rule.border_size, Some(3));
    assert_eq!(rule.no_border, Some(true), "`border:false` is `no_border`");
    assert_eq!(rule.no_shadow, Some(true));
    assert_eq!(rule.no_rounding, Some(true));
    assert_eq!(rule.decorate, Some(true));
    assert_eq!(rule.layout.as_deref(), Some("master"));
    assert_eq!(
        rule.layout_options,
        [("orientation".to_owned(), "top".to_owned())]
    );
    assert_eq!(rule.default_name.as_deref(), Some("code"));
    assert_eq!(rule.is_persistent, Some(true));
    assert_eq!(rule.on_created_empty.as_deref(), Some("foot"));
    assert_eq!(rule.animation.as_deref(), Some("slide"));

    // `special:` names a scratchpad, and a bare word that is not a number
    // is a name, which is what Hyprland's `getWorkspaceIDNameFromString`
    // does with one.
    assert_eq!(
        WorkspaceRule::parse("special:magic, gapsout:0").map(|rule| rule.which),
        Ok(Which::Special("magic".to_owned()))
    );
    assert_eq!(
        WorkspaceRule::parse("code, gapsout:0").map(|rule| rule.which),
        Ok(Which::Name("code".to_owned()))
    );

    // A key that is not one of Hyprland's is skipped rather than refused:
    // its chain of `find`s falls through and the rule is left as it was.
    let rule = WorkspaceRule::parse("2, wobble:7, bordersize:1").expect("a rule");
    assert_eq!(rule.border_size, Some(1));

    assert!(WorkspaceRule::parse("").is_err(), "no workspace");
    assert!(
        WorkspaceRule::parse("2, bordersize:wide").is_err(),
        "not a number"
    );
}

/// Every rule that matches one workspace is applied, later lines winning.
#[test]
fn the_rules_that_match_a_workspace_are_gathered() {
    use crate::{WorkspaceRule, rules_for};

    let rules: Vec<WorkspaceRule> = [
        "1, gapsout:0",
        "1, bordersize:0",
        "1, bordersize:4",
        "name:code, gapsin:1",
    ]
    .iter()
    .map(|line| WorkspaceRule::parse(line).expect("a rule"))
    .collect();

    let first = rules_for(&rules, 1, "1");
    assert_eq!(first.gaps_out, Some(Gaps::all(0)));
    assert_eq!(first.border_size, Some(4), "the later line wins");
    assert_eq!(first.gaps_in, None);

    // A rule written with a name matches by name, not by number.
    assert_eq!(rules_for(&rules, 7, "code").gaps_in, Some(Gaps::all(1)));
    assert_eq!(rules_for(&rules, 2, "2"), WorkspaceRule::default());
}

/// A `binds:` option is an option, not a `bind` with flags.
///
/// `bind` takes its flags as letters glued to the keyword -- `bindl`,
/// `bindrm`, `bindel` -- and Hyprland also has a whole option category
/// called `binds`. The two begin with the same four letters, so a parser
/// that reached for the bind first answered `binds:workspace_back_and_forth`
/// with `invalid flag :` and dropped the line. A keyword never has a colon
/// in it and an option always does, which is what tells them apart.
#[test]
fn a_binds_option_is_not_a_bind_with_flags() {
    let parsed = parse(
        "t.conf",
        "binds:workspace_back_and_forth = true\n\
         binds:hide_special_on_workspace_change = 1\n\
         bindl = , XF86AudioPlay, exec, playerctl play-pause\n",
        &mut NoSources,
    );
    assert_eq!(parsed.diagnostics, []);
    assert_eq!(
        parsed.config.bool("binds:workspace_back_and_forth"),
        Some(true)
    );
    assert_eq!(
        parsed.config.bool("binds:hide_special_on_workspace_change"),
        Some(true)
    );
    assert_eq!(parsed.config.binds.len(), 1, "and the bind is still a bind");
    assert!(parsed.config.binds[0].flags.locked);
}

/// `monitor`, `no_initial_focus` and `nearest_neighbor`, the last three
/// effects a real configuration is likely to write.
///
/// `no_initial_focus` is `no_focus` by another name: Hyprland keeps the two
/// apart because one is checked when the window maps and the other whenever
/// it would be focused, and this compositor applies its rules when a window
/// maps, which is the same moment.
#[test]
fn the_effects_that_place_a_window_and_sample_it() {
    let effects = |line: &str| WindowRule::parse(line).map(|rule| rule.effects);

    assert_eq!(
        effects("monitor DP-1"),
        Ok(vec![Effect::Monitor("DP-1".to_owned())])
    );
    assert_eq!(
        effects("monitor desc:Dell Inc. DELL P2418D"),
        Ok(vec![Effect::Monitor(
            "desc:Dell Inc. DELL P2418D".to_owned()
        )])
    );
    assert!(effects("monitor").is_err(), "it takes a monitor");

    assert_eq!(effects("no_initial_focus"), Ok(vec![Effect::NoFocus]));
    assert_eq!(effects("no_focus"), Ok(vec![Effect::NoFocus]));

    assert_eq!(
        effects("nearest_neighbor"),
        Ok(vec![Effect::NearestNeighbor(true)])
    );
    assert_eq!(
        effects("nearest_neighbor false"),
        Ok(vec![Effect::NearestNeighbor(false)])
    );
}

/// `group ...` and `no_close_for`, the two effects that are bookkeeping
/// rather than an action.
///
/// `group`'s words each turn one flag on, `always` qualifies whichever came
/// before it, and `override` or `unset` clear everything set so far -- which
/// is how a later rule undoes an earlier one
/// (`CWindow::applyDynamicRules`).
#[test]
fn the_group_words_and_no_close_for() {
    use crate::GroupRules;

    let group = |line: &str| match WindowRule::parse(line).map(|rule| rule.effects) {
        Ok(effects) => effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Group(rules) => Some(rules),
                _ => None,
            })
            .expect("a group effect"),
        Err(why) => panic!("{line}: {why}"),
    };

    assert_eq!(
        group("group set"),
        GroupRules {
            set: true,
            ..GroupRules::default()
        }
    );
    // `new` is Hyprland's shorthand for `barred set`.
    assert_eq!(
        group("group new"),
        GroupRules {
            set: true,
            barred: true,
            ..GroupRules::default()
        }
    );
    assert_eq!(
        group("group set always"),
        GroupRules {
            set: true,
            always: true,
            ..GroupRules::default()
        }
    );
    assert_eq!(
        group("group lock invade deny"),
        GroupRules {
            lock: true,
            invade: true,
            deny: true,
            ..GroupRules::default()
        }
    );
    // `override` and `unset` clear what came before them.
    assert_eq!(
        group("group set lock override"),
        GroupRules {
            cleared: true,
            ..GroupRules::default()
        }
    );
    assert_eq!(
        group("group unset set"),
        GroupRules {
            cleared: true,
            ..GroupRules::default()
        },
        "`unset` stops reading, so the `set` after it is not read"
    );
    // A bare `group` is the whole effect and asks for nothing in
    // particular, which Hyprland treats as "leave the flags alone".
    assert_eq!(group("group"), GroupRules::default());

    assert_eq!(
        WindowRule::parse("no_close_for 3000").map(|rule| rule.effects),
        Ok(vec![Effect::NoCloseFor(3000)])
    );
    assert!(WindowRule::parse("no_close_for soon").is_err());
}

#[test]
fn a_monitor_named_by_a_rule_is_not_the_catch_alls_even_above_it() {
    // A configuration written for three monitors, as Hyprland's own users
    // write one: a line for each by its description, then a catch-all for
    // any other. Hyprland takes the named rule wherever the catch-all is.
    let rules: Vec<MonitorRule> = [
        "desc:Dell Inc. DELL P2418D MY3ND91J09CT, preferred, 0x0, 1",
        "desc:Lenovo Group Limited R27qe Gen2 UTP03KBB, preferred, 2560x0, 1",
        ", preferred, auto, 1",
    ]
    .iter()
    .map(|line| MonitorRule::parse(line).unwrap())
    .collect();
    let lenovo = MonitorRule::for_monitor(
        &rules,
        "Virtual-1",
        "Lenovo Group Limited R27qe Gen2 UTP03KBB",
    )
    .unwrap();
    assert_eq!(lenovo.position, Position::At(2560, 0));
    // A monitor no line names gets the catch-all.
    let other = MonitorRule::for_monitor(&rules, "Virtual-1", "").unwrap();
    assert_eq!(other.position, Position::Auto);
    // Of two catch-alls the later wins, as of two rules for one connector.
    let twice: Vec<MonitorRule> = [", 1024x768@60, auto, 1", ", preferred, auto, 2"]
        .iter()
        .map(|line| MonitorRule::parse(line).unwrap())
        .collect();
    let taken = MonitorRule::for_monitor(&twice, "Virtual-1", "").unwrap();
    assert_eq!(taken.mode, Mode::Preferred);
    let named: Vec<MonitorRule> = [
        "Virtual-1, 1024x768@60, 0x0, 1",
        ", preferred, auto, 1",
        "Virtual-1, preferred, 100x0, 1",
    ]
    .iter()
    .map(|line| MonitorRule::parse(line).unwrap())
    .collect();
    let taken = MonitorRule::for_monitor(&named, "Virtual-1", "").unwrap();
    assert_eq!(taken.position, Position::At(100, 0));
    assert_eq!(MonitorRule::for_monitor(&[], "Virtual-1", ""), None);
}
