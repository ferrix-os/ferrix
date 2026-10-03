# Ferrix press and brand kit

Ferrix is an experimental Rust operating system. Its own kernel runs tested
Linux programs without changing the binaries, while disk, network, graphics
and input drivers run as restartable processes.

## Copy you can use

**Short:** Ferrix runs Linux apps without Linux. It has its own Rust kernel and
drivers that run outside the kernel.

**Longer:** Ferrix is an experimental operating system written in Rust. You can
boot its Wayland desktop in QEMU and run unmodified Linux programs such as
Chrome, `git`, `curl` and `rustc`. Its disk, network, graphics and input
drivers run as separate processes that can restart after a crash. Ferrix is a
research project; authentication and parts of process isolation are still in
progress.

The [README](../../README.md) has boot commands. The [technical guide](../GUIDE.md)
shows how to reproduce the compiler and image-build tests. Check the
[roadmap](../roadmap/where-it-stands.md) before describing work in
progress as finished.

## Logo and images

- [Logo mark (SVG)](logo.svg): the vector mark on a transparent background.
- [Dark banner](../brand/banner-dark.png) and [light banner](../brand/banner-light.png): README headers.
- [Social preview](../brand/social-preview.png): a wide image for shared links.
- [Screenshots](../brand/screenshots/CAPTIONS.md): real captures, with notes on how they were made.
- [Favicon](../brand/favicon.svg): the mark on a dark tile, for small icons.

Use the SVG mark with the word “Ferrix” set beside it. Keep its proportions,
leave some space around it, and use it on a background where the grey outline
remains visible. The mark is the existing Ferrix logo; it is not a substitute
for the project's name in running text. The source asset also lives at
[`docs/brand/logo-mark.svg`](../brand/logo-mark.svg); the two SVG files
should remain identical.

## Visual identity

| | Dark | Light | Use |
|---|---|---|---|
| Background | `#0d0f12` | `#f7f5f2` | Page background |
| Surface | `#161a20` | `#ffffff` | Panels and cards |
| Text | `#eef1f5` | `#16181c` | Main copy |
| Muted text | `#9aa4b2` | `#5b6470` | Captions |
| Rust orange | `#ff7a2b` | `#c64a06` | Links and highlights |

Use Inter for display and body text. Use JetBrains Mono for code on the web;
the generated banners use Liberation Mono. The fonts bundled in
[`assets/fonts`](../../assets/fonts) are the ones used for repository artwork.
The [brand notes](../brand/BRAND.md) cover the image sources and other
colour tokens.

## Voice

Write as if you are showing someone the system at your desk. Say what runs,
how to try it and what still fails. Give a command, screenshot or source link
for a technical claim. Avoid grand claims about changing computing or
“redefining” operating systems. “Linux apps without Linux” is a short hook,
not a promise that every Linux program works. Mention the Claude-led build
process when explaining the project, after explaining what Ferrix does.

For a press mention, link to the [website](https://ferrix-os.github.io/)
and [source repository](https://github.com/ferrix-os/ferrix). Ferrix is MIT
licensed; see [LICENSE](../../LICENSE) for the terms.
