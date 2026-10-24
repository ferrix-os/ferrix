# Fonts

The faces a Ferrix desktop draws text in, carried in the tree so that no
image depends on a font being installed anywhere else. `cargo xtask` puts
them in `/usr/share/ferrix/fonts` on every image that runs Chrome in a window
(`run-compositor --chrome`, `test-chrome-window`, `bench-chrome`), with
`fonts.conf` beside them and `FONTCONFIG_FILE` naming it.

| directory | face | version | licence |
|---|---|---|---|
| `inter/` | Inter, variable, upright and italic | 4.1 | SIL Open Font License 1.1, `inter/LICENSE` |
| `liberation/` | Liberation Sans, Serif and Mono, four styles each | 2.1.5 | SIL Open Font License 1.1, `liberation/LICENSE` |
| `noto-cjk/` | Noto Sans CJK, variable, Japanese, Korean, Simplified, Traditional and Hong Kong Chinese | 2.004 | SIL Open Font License 1.1, `noto-cjk/LICENSE` |
| `phosphor/` | Phosphor icons, regular weight, as a font: one glyph an icon in the Private Use Area | 2.1.1 | MIT, `phosphor/LICENSE` |

**Inter** is fontconfig's sans-serif: what a program gets for `sans`,
`sans-serif` or `system-ui`, Chrome's own tabs, toolbar and menus among
them. It was drawn for screens, and its variable file carries every weight
from Thin to Black in 0.9 MB. A page's CSS `sans-serif` is Chrome's own
preference, Arial on Linux, and so Liberation Sans.

**Liberation** is metric-compatible with Arial, Times New Roman and Courier
New: every glyph is as wide as the face it stands in for, so a page laid out
for those does not reflow. Chrome's default fonts on Linux are those three,
and a great many pages name them; fontconfig's `30-metric-aliases.conf`
answers them with Liberation.

**Noto Sans CJK** draws Chinese, Japanese and Korean, which neither of the
others has a glyph for: without it, a page's title in Japanese is a row of
boxes. Nothing names it; fontconfig offers it to Chrome as the fallback for
the characters the first choice lacks. The one collection holds every
weight from Thin to Black for all five regions, which share most of their
glyphs, in 33 MB, and fontconfig picks the region by the page's language.

**Phosphor** is the icon set of the Yoru desktop (`docs/DESKTOP.md` §6),
as its design system asks: the dock's, the title bars' and the widgets'
icons are its glyphs, drawn as text in the colour of the text around them.
A program names an icon by Phosphor's name and looks its code point up in
the package's `style.css` (`.ph-terminal-window:before { content: "\e...";
}`), which is not carried: the programs keep the few they draw.

The OFL lets the three text faces be bundled and redistributed with software, provided the
licence travels with them and they are not sold on their own. It does, in
each directory.

## Where they come from

Each file is unchanged from its project's release archive:

| archive | SHA-256 |
|---|---|
| [`Inter-4.1.zip`](https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip) | `9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e` |
| [`liberation-fonts-ttf-2.1.5.tar.gz`](https://github.com/liberationfonts/liberation-fonts/files/7261482/liberation-fonts-ttf-2.1.5.tar.gz) | `7191c669bf38899f73a2094ed00f7b800553364f90e2637010a69c0e268f25d0` |
| [`01_NotoSansCJK-OTF-VF.zip`](https://github.com/notofonts/noto-cjk/releases/download/Sans2.004/01_NotoSansCJK-OTF-VF.zip) | `d5e33aebad9f8a0c0896a4a29199ef85ca966134db164426c74e83e6f13c43cd` |
| [`@phosphor-icons/web-2.1.1.tgz`](https://registry.npmjs.org/@phosphor-icons/web/-/web-2.1.1.tgz) | `c74ab1decb33d7cefa22a7c412345e30d28ac39941b3be6837bc956cbef654d0` |

`inter/LICENSE` is the archive's `LICENSE.txt`.
`phosphor/Phosphor.ttf` and `phosphor/LICENSE` are the package's
`src/regular/Phosphor.ttf` and `LICENSE`; MIT asks only that the notice
travel with the file, which it does.
`noto-cjk/NotoSansCJK-VF.otf.ttc` is the archive's
`Variable/OTC/NotoSansCJK-VF.otf.ttc`.

## `fonts.conf`

It adds this directory to fontconfig's, makes Inter the first choice for the
three sans-serif names, and asks for greyscale antialiasing with slight
hinting. Then it includes `/etc/fonts/fonts.conf`, so everything the system
configures still applies. The file's own comments say why its edits come
before that include.
