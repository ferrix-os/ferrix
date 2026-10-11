//! Icon lookup against fuzzel's `icon.c`.

use super::{DirType, Kind, icon_dirs, load_themes, lookup, parse_theme};
use crate::testdir::TestDir;

const HICOLOR: &str = "\
[Icon Theme]
Name=Hicolor
Directories=16x16/apps,32x32/apps,48x48/apps,scalable/apps,32x32/actions,32x32@2/apps

[16x16/apps]
Size=16
Context=Applications
Type=Threshold

[32x32/apps]
Size=32
Context=Applications
Type=Threshold

[48x48/apps]
Size=48
Context=Applications
Type=Fixed

[scalable/apps]
MinSize=1
Size=128
MaxSize=256
Context=Applications
Type=Scalable

[32x32/actions]
Size=32
Context=Actions

[32x32@2/apps]
Size=32
Scale=2
Context=Applications
";

#[test]
fn a_theme_index() {
    let (dirs, inherits) = parse_theme(HICOLOR, true);
    let names: Vec<&str> = dirs.iter().map(|d| d.path.as_str()).collect();
    // The actions directory is filtered out by its context.
    assert_eq!(
        names,
        vec![
            "16x16/apps",
            "32x32/apps",
            "48x48/apps",
            "scalable/apps",
            "32x32@2/apps"
        ]
    );
    assert!(inherits.is_empty());
    let scalable = dirs.get(3).cloned();
    assert_eq!(
        scalable.map(|d| (d.kind, d.min_size, d.size, d.max_size)),
        Some((DirType::Scalable, 1, 128, 256))
    );
    // In dmenu mode every context is searched.
    let (all, _) = parse_theme(HICOLOR, false);
    assert_eq!(all.len(), 6);
    let (_, inherits) = parse_theme("[Icon Theme]\nInherits=Adwaita,hicolor\n", true);
    assert_eq!(inherits, vec!["Adwaita", "hicolor"]);
}

#[test]
fn the_nearest_size_and_the_fallbacks() {
    let dir = TestDir::new("icons");
    let icons = dir.path().join("share/icons");
    let _ = dir.write("share/icons/hicolor/index.theme", HICOLOR);
    let _ = dir.write("share/icons/hicolor/16x16/apps/term.png", "png");
    let _ = dir.write("share/icons/hicolor/48x48/apps/term.png", "png");
    let _ = dir.write("share/icons/hicolor/scalable/apps/chrome.svg", "svg");
    let _ = dir.write("share/icons/hicolor/32x32/apps/zinc.png", "png");
    let _ = dir.write("share/icons/hicolor/32x32/apps/zinc.svg", "svg");
    let _ = dir.write(
        "share/icons/default/index.theme",
        "[Icon Theme]\nInherits=Mine\nDirectories=\n",
    );
    let _ = dir.write(
        "share/icons/Mine/index.theme",
        "[Icon Theme]\nDirectories=apps/32\n[apps/32]\nSize=32\nContext=Apps\n",
    );
    let _ = dir.write("share/icons/Mine/apps/32/term.svg", "svg");
    let _ = dir.write("share/icons/lonely.png", "png");
    let data = vec![dir.path().join("share")];
    let dirs = icon_dirs(&data, None);
    assert_eq!(dirs.first(), Some(&icons));
    let themes = load_themes("default", &dirs, true);
    let order: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(order, vec!["default", "Mine", "hicolor"]);
    let names = [
        Some("term"),
        Some("chrome"),
        Some("zinc"),
        Some("lonely"),
        Some("missing"),
        None,
        Some("/abs/icon.svg"),
        Some("/abs/icon.xpm"),
    ];
    let found = lookup(&themes, &dirs, 30, &names);
    let shown: Vec<Option<String>> = found
        .iter()
        .map(|f| {
            f.as_ref().map(|f| {
                f.path
                    .strip_prefix(&icons)
                    .unwrap_or(&f.path)
                    .display()
                    .to_string()
            })
        })
        .collect();
    assert_eq!(
        shown,
        vec![
            // The inherited theme comes before hicolor, and 30 is within
            // its 32's threshold.
            Some("Mine/apps/32/term.svg".to_owned()),
            Some("hicolor/scalable/apps/chrome.svg".to_owned()),
            // A PNG before an SVG of the same name.
            Some("hicolor/32x32/apps/zinc.png".to_owned()),
            // Not in a theme: the bare directory.
            Some("lonely.png".to_owned()),
            None,
            None,
            Some("/abs/icon.svg".to_owned()),
            None,
        ]
    );
    assert_eq!(
        found.first().and_then(|f| f.as_ref().map(|f| f.kind)),
        Some(Kind::Svg)
    );
    // At 20 pixels hicolor alone: 16 is within threshold 2? No -- 20 is
    // above 18, so the nearest, 16x16 (diff 4) beats the fixed 48 (28).
    let hicolor = load_themes("hicolor", &dirs, true);
    let found = lookup(&hicolor, &dirs, 20, &[Some("term")]);
    assert_eq!(
        found.first().cloned().flatten().map(|f| f.path),
        Some(icons.join("hicolor/16x16/apps/term.png"))
    );
}
