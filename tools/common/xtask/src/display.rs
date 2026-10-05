//! `cargo xtask test-display`: iteration 1 of the compositor, a blank screen
//! on Ferrix, judged pixel by pixel.
//!
//! `docs/DISPLAY.md` §3. The compositor's first program, `src/user/system/linux/compositor/blank`,
//! is built static for the architecture and booted as init with a virtio-gpu
//! device on the bus. It sets the connector's preferred mode and fills a dumb
//! buffer with one colour, then prints [`MARKER`]. At that line this module
//! asks QEMU, over its QMP socket, for a screendump of the virtio-gpu head,
//! and requires every pixel to be that colour.
//!
//! The marker line also names the plane the program found on the card after
//! its modeset, read the way Smithay's legacy path reads planes: the test
//! requires it to be a primary plane, so a card whose planes or `type`
//! property break fails here before a compositor panics on it
//! (`docs/DISPLAY.md` §2.3, E4).
//!
//! A check that cannot fail proves nothing, so the test runs twice: once as
//! above, and once with the program built with `negative-control`, which
//! draws pixel (0, 0) in another colour. The second boot must fail the check
//! on exactly that pixel and no other.
//!
//! QMP is JSON lines over a TCP socket on localhost, on every host, so this
//! works the same on Linux and Windows. xtask takes no crates, so what QMP
//! needs of JSON is written here: two commands out and a `return` or `error`
//! back. The screendump is QEMU's default format, binary PPM.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result};

/// What `--init` names `src/user/system/linux/compositor/blank` by, built for the architecture
/// first, so `run --display --init blank` shows its screen in a window.
pub(crate) const INIT_NAME: &str = "blank";

/// What the program prints once the colour is on the screen. The same string
/// as `src/user/system/linux/compositor/blank/src/card.rs`'s `MARKER`.
pub(crate) const MARKER: &str = "compositor: scanout";

/// What the program prints when it could not.
pub(crate) const FAILED: &str = "compositor: failed";

/// What the marker line ends with when the card has a primary plane for the
/// CRTC the program set: `plane <id> Primary`.
const PRIMARY: &str = "Primary";

/// What both of its lines start with, which is what the boot is watched for.
const EITHER: &str = "compositor: ";

/// The colour the program fills with, as red, green and blue: its
/// `BACKGROUND`, `0x1E1E2E`.
pub(crate) const BACKGROUND: [u8; 3] = [0x1E, 0x1E, 0x2E];

/// The virtio-gpu device's QEMU id, which the screendump names.
pub(crate) const DEVICE_ID: &str = "gpu0";

/// The id of screen `index`'s device: `gpu0`, `gpu1`.
pub(crate) fn device_id(index: u32) -> String {
    format!("gpu{index}")
}

/// How long the screen may take to show the colour after the marker: the
/// program's `SETCRTC` returns once the flush is queued, not once QEMU has
/// drawn it.
const SETTLE: Duration = Duration::from_secs(5);

/// The Rust target the program is built for on `arch`, if the architecture
/// has virtio-gpu.
pub(crate) fn target(arch: Arch) -> Option<&'static str> {
    match arch {
        Arch::X86_64 => Some("x86_64-unknown-linux-musl"),
        Arch::AArch64 => Some("aarch64-unknown-linux-musl"),
        Arch::Armv7a => Some("armv7-unknown-linux-musleabihf"),
    }
}

/// Build `src/user/system/linux/compositor/blank` for `arch`, with the negative control or without,
/// and return where the program is.
pub(crate) fn build_blank(arch: Arch, negative: bool) -> Result<PathBuf> {
    let target = target(arch).ok_or_else(|| {
        Error::new(format!(
            "{arch} has no virtio-gpu in QEMU; the display test runs on x86_64 and aarch64"
        ))
    })?;
    let flavour = if negative { "negative" } else { "plain" };
    let target_dir = paths::target_dir().join("compositor").join(flavour);
    println!("  building src/user/system/linux/compositor/blank ({flavour}) for {target}");
    let program = target_dir.join(target).join("release").join("blank");
    let mut build = crate::builds::Build::cargo(
        format!(
            "cargo build (src/user/system/linux/compositor/blank, {flavour}) --target {target}"
        ),
        paths::workspace_root().join("src/user/system/linux/compositor"),
    )
    .args([
        "build",
        "--release",
        "-p",
        "compositor-blank",
        "--target",
        target,
    ])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program);
    if negative {
        build = build.args(["--features", "negative-control"]);
    }
    build.run()?;
    Ok(program)
}

/// A free TCP port on localhost for QEMU's QMP server. The listener is closed
/// before QEMU binds the port, so another program could take it in between;
/// QEMU then fails to start and says why.
pub(crate) fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

/// A QMP session.
pub(crate) struct Qmp {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Qmp {
    /// Connect to QEMU's QMP server on `port`, retrying until `deadline`,
    /// read its greeting and leave capabilities negotiation.
    pub(crate) fn connect(port: u16, deadline: Instant) -> Result<Self> {
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let stream = loop {
            match TcpStream::connect(address) {
                Ok(stream) => break stream,
                Err(error) if Instant::now() >= deadline => {
                    return Err(Error::new(format!(
                        "could not reach QMP on {address}: {error}"
                    )));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let writer = stream.try_clone()?;
        let mut session = Self {
            reader: BufReader::new(stream),
            writer,
        };
        let greeting = session.line()?;
        if !greeting.contains("\"QMP\"") {
            return Err(Error::new(format!("QMP greeted with `{greeting}`")));
        }
        let _ = session.execute("qmp_capabilities", None)?;
        Ok(session)
    }

    fn line(&mut self) -> Result<String> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(Error::new("QMP closed the connection"));
        }
        Ok(line)
    }

    /// Run `command` with `arguments`, a JSON object's text, and return the
    /// reply line, skipping the asynchronous events QEMU interleaves.
    pub(crate) fn execute(&mut self, command: &str, arguments: Option<&str>) -> Result<String> {
        let request = match arguments {
            Some(arguments) => format!("{{\"execute\":\"{command}\",\"arguments\":{arguments}}}\n"),
            None => format!("{{\"execute\":\"{command}\"}}\n"),
        };
        self.writer.write_all(request.as_bytes())?;
        loop {
            let line = self.line()?;
            if line.contains("\"event\"") {
                continue;
            }
            if line.contains("\"error\"") {
                return Err(Error::new(format!(
                    "QMP `{command}` failed: {}",
                    line.trim()
                )));
            }
            if line.contains("\"return\"") {
                return Ok(line);
            }
        }
    }

    /// Every virtual processor's registers, as the human monitor's `info
    /// registers -a` prints them: where each one is, for a kernel that
    /// panicked because another processor stopped answering it. The one that
    /// stopped answering is still running where it stuck, and only this
    /// sees it from outside, since a processor spinning with interrupts
    /// masked takes no request to say where it is.
    pub(crate) fn registers(&mut self) -> Result<String> {
        let reply = self.execute(
            "human-monitor-command",
            Some("{\"command-line\":\"info registers -a\"}"),
        )?;
        // The reply is one JSON string; its escaped newlines are the lines.
        Ok(reply.replace("\\r\\n", "\n").replace("\\n", "\n"))
    }

    /// Ask for a screendump of `device`'s first head, or of QEMU's first
    /// console when `device` is `None`, into `file`.
    pub(crate) fn screendump(&mut self, device: Option<&str>, file: &Path) -> Result<()> {
        let file = json_string(&file.display().to_string());
        let arguments = match device {
            Some(device) => format!(
                "{{\"filename\":{file},\"device\":{},\"head\":0}}",
                json_string(device)
            ),
            None => format!("{{\"filename\":{file}}}"),
        };
        self.execute("screendump", Some(&arguments)).map(drop)
    }

    /// Put `events` -- each one an `InputEvent` object's JSON -- into the
    /// guest's input devices, as one report.
    ///
    /// QEMU routes each event to a device that takes its kind, and ends the
    /// lot with a sync of its own, so one call is one report.
    ///
    /// # Errors
    ///
    /// Whatever QMP said, which for a guest with no device that takes the
    /// event is `Input handler not found`.
    pub(crate) fn input_send_event(&mut self, events: &[String]) -> Result<()> {
        let arguments = format!("{{\"events\":[{}]}}", events.join(","));
        self.execute("input-send-event", Some(&arguments)).map(drop)
    }
}

/// `text` as a JSON string literal.
pub(crate) fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A screendump: width, height, and three bytes a pixel, red first.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Image {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixels: Vec<u8>,
}

/// Parse a binary PPM (`P6`) with a maximum value of 255.
pub(crate) fn parse_ppm(bytes: &[u8]) -> Result<Image> {
    let bad = |why: &str| Error::new(format!("not a screendump PPM: {why}"));
    let mut at = 0;
    let mut fields = Vec::new();
    while fields.len() < 4 {
        // Whitespace and comments between the header's fields.
        while let Some(&byte) = bytes.get(at) {
            if byte == b'#' {
                while bytes.get(at).is_some_and(|&b| b != b'\n') {
                    at += 1;
                }
            } else if byte.is_ascii_whitespace() {
                at += 1;
            } else {
                break;
            }
        }
        let start = at;
        while bytes.get(at).is_some_and(|b| !b.is_ascii_whitespace()) {
            at += 1;
        }
        let field = bytes
            .get(start..at)
            .filter(|field| !field.is_empty())
            .ok_or_else(|| bad("the header ends early"))?;
        fields.push(String::from_utf8_lossy(field).into_owned());
    }
    // Exactly one whitespace byte separates the header from the pixels.
    at += 1;
    let [magic, width, height, max] = fields.as_slice() else {
        return Err(bad("the header ends early"));
    };
    if magic != "P6" {
        return Err(bad("not P6"));
    }
    let number = |field: &str| {
        field
            .parse::<usize>()
            .map_err(|_| bad("a size is not a number"))
    };
    let (width, height) = (number(width)?, number(height)?);
    if max != "255" {
        return Err(bad("the maximum value is not 255"));
    }
    let len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| bad("the size overflows"))?;
    let pixels = at
        .checked_add(len)
        .and_then(|end| bytes.get(at..end))
        .ok_or_else(|| bad("fewer pixels than the header says"))?
        .to_vec();
    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// Every pixel that is not `color`, as `(x, y, [r, g, b])`, up to `limit`,
/// and how many there are in all.
pub(crate) fn mismatches(
    image: &Image,
    color: [u8; 3],
    limit: usize,
) -> (Vec<(usize, usize, [u8; 3])>, usize) {
    let width = image.width.max(1);
    let mut found = Vec::new();
    let mut count = 0;
    for (index, pixel) in image.pixels.chunks_exact(3).enumerate() {
        if let [red, green, blue] = *pixel
            && [red, green, blue] != color
        {
            count += 1;
            if found.len() < limit {
                found.push((index % width, index / width, [red, green, blue]));
            }
        }
    }
    (found, count)
}

/// The three pixels the render probe reads back after drawing: see
/// `src/user/system/linux/compositor/drm`'s `drew`.
const DREW: &str = "0xff0000 0x00ff00 0xff0000";

/// How many bytes of the Venus blob the render probe writes and must read
/// back through its mapping: a page, `src/user/system/linux/compositor/drm`'s `PROBE_BYTES`.
const BLOB: &str = "4096";

/// What the program prints about the render node, before its own marker.
pub(crate) const RENDER: &str = "render:";

/// The driver the render line names, if it found a node: the line is
/// `render: renderD128 <driver> 3d <n> capsets 0x<mask> object <h>/<r> of <n>
/// bytes`, and `render: none <why>` when there was none. Read from the marker
/// rather than from the start, because a console line carries a timestamp
/// before it.
pub(crate) fn render_driver(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(RENDER)?;
    let mut words = rest.split_whitespace();
    let node = words.next()?;
    if !node.starts_with("renderD") {
        return None;
    }
    words.next().filter(|driver| !driver.is_empty())
}

/// The resource the render line says it made: `<handle>/<resource>`, from the
/// `object` word onwards. `None` when the program could not make one, which
/// includes it saying `object none <why>`.
///
/// A node that names a driver has only proved the core was told about one:
/// the name comes from the HELLO. Making a resource is what proves the rest
/// of the path -- the handle table, the session, the driver and the device --
/// so the 3D boot judges this as well (`docs/GPU.md` step 2).
pub(crate) fn render_object(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(RENDER)?;
    let mut words = rest.split_whitespace().skip_while(|word| *word != "object");
    let _ = words.next()?;
    words.next().filter(|made| made.contains('/'))
}

/// What follows `word` on the render line, when it is not `none`: the
/// capability set's size after `caps`, the bytes that came back after
/// `moved`. `None` when the program could not do it, which it says as
/// `<word> none <why>`.
pub(crate) fn render_did<'a>(line: &'a str, word: &str) -> Option<&'a str> {
    let (_, rest) = line.split_once(RENDER)?;
    let mut words = rest.split_whitespace().skip_while(|found| *found != word);
    let _ = words.next()?;
    words.next().filter(|said| *said != "none")
}

/// What the card's reads must say on the marker line (`compositor_drm`'s
/// `events`): `EAGAIN` for a read of 8 with nothing queued, 0 for a read of
/// 8 with a flip's event queued, which leaves it there, and the 32-byte event
/// for a read of 4096 after, as Linux's `drm_read` answers.
const CARD_EVENTS: &str = "EAGAIN 0 32";

/// The three words after `events` on the marker line, if it has them.
pub(crate) fn card_events(line: &str) -> Option<String> {
    let mut words = line.split_whitespace().skip_while(|word| *word != "events");
    let _ = words.next()?;
    let said: Vec<&str> = words.take(3).collect();
    (said.len() == 3).then(|| said.join(" "))
}

/// The id of the primary plane the marker line names, if it names one: its
/// last three words are `plane <id> Primary`.
pub(crate) fn primary_plane(line: &str) -> Option<u32> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        [.., "plane", id, kind] if *kind == PRIMARY => id.parse().ok(),
        _ => None,
    }
}

/// Take a screendump of `device` into `file` and parse it.
fn read_dump(qmp: &mut Qmp, device: Option<&str>, file: &Path) -> Result<Image> {
    let _ = std::fs::remove_file(file);
    qmp.screendump(device, file)?;
    let bytes = std::fs::read(file)
        .map_err(|error| Error::new(format!("reading {}: {error}", file.display())))?;
    parse_ppm(&bytes)
}

/// Judge the render node's line from a boot's `lines`.
///
/// `--gl` puts a GPU behind the card, and then `/dev/dri/renderD128` must be
/// there, name the driver serving it, and have done each thing the probe
/// tries; without `--gl` there is no GPU, no node, and the program must say
/// so. Each boot is the other's control: a check that cannot fail proves
/// nothing (`docs/GPU.md` step 2).
fn judge_render(arch: Arch, gl: bool, venus: bool, lines: &[String]) -> Result<()> {
    let render = lines
        .iter()
        .rev()
        .find(|line| line.contains(RENDER))
        .map_or("", |line| line.trim());
    match (gl, render_driver(render)) {
        (true, Some(driver)) => {
            let Some(made) = render_object(render) else {
                return Err(Error::new(format!(
                    "{arch}: the render node `{driver}` made no resource: `{render}`"
                )));
            };
            println!("  {arch}: the render node is `{driver}`, and made resource {made}");
            // The rest of step 2, each proved by the device and not by
            // the node's own tables: the capability set came from it,
            // and bytes went to it and came back.
            for (word, what) in [
                ("caps", "read no capability set"),
                ("moved", "moved no bytes to the device and back"),
                ("drew", "drew nothing on the GPU"),
            ] {
                let Some(said) = render_did(render, word) else {
                    return Err(Error::new(format!(
                        "{arch}: the render node `{driver}` {what}: `{render}`"
                    )));
                };
                println!("  {arch}: the render node: {word} {said}");
            }
            // And what it drew: red where the clear was left, green in
            // the top right quarter where the rectangle went, red below
            // it. The third is what says rows are read back the way
            // they were drawn.
            let (_, after) = render.split_once("drew ").unwrap_or_default();
            let picture = after
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            if picture != DREW {
                return Err(Error::new(format!(
                    "{arch}: the GPU drew `{picture}`, not `{DREW}`: `{render}`"
                )));
            }
            // And a read with nothing queued is Linux's `drm_read`:
            // `EAGAIN` under `O_NONBLOCK`, on a render node as on a card.
            if render_did(render, "read") != Some("EAGAIN") {
                return Err(Error::new(format!(
                    "{arch}: a non-blocking read of the render node was not EAGAIN: `{render}`"
                )));
            }
            println!("  {arch}: the render node: read EAGAIN");
            // With Venus on the card, a blob of host memory made in a
            // Venus context and mapped through the device's window: every
            // byte written there reads back (`docs/GPU.md` §6.1).
            if venus {
                let blob = render_did(render, "blob");
                if blob != Some(BLOB) {
                    return Err(Error::new(format!(
                        "{arch}: the Venus blob gave back {} bytes of {BLOB}: `{render}`",
                        blob.unwrap_or("none")
                    )));
                }
                println!("  {arch}: the render node: a Venus blob, {BLOB} bytes mapped");
            }
        }
        (true, None) => {
            return Err(Error::new(format!(
                "{arch}: the 3D card has no render node: `{render}`"
            )));
        }
        (false, Some(driver)) => {
            return Err(Error::new(format!(
                "{arch}: a card with no GPU answered a render node `{driver}`"
            )));
        }
        (false, None) => {
            println!("  {arch}: no render node, as a card with no GPU has none");
        }
    }
    Ok(())
}

/// Boot `arch` with `program` as init and a virtio-gpu, and return the
/// screendump taken once the program has printed its marker and the screen
/// has had [`SETTLE`] to show it — or as soon as it is all `BACKGROUND`.
///
/// `None` for a `--gl` boot that was judged without one.
fn boot_and_dump(arch: Arch, program: &Path, args: &Args, name: &str) -> Result<Option<Image>> {
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel_with_init(arch, args.release, program, "")?;
    let natives = crate::native::build(arch, args.release)?;
    let image = crate::fat::write_image(arch, &loader, &kernel, &natives, None)?;

    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join(format!("{name}.ppm"));
    let mut taken = None;
    let mut judged_without_a_dump = false;
    let hook = |watching: &mut crate::qemu::Watching<'_>| -> Result<()> {
        let lines = watching.lines();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        if let Some(line) = lines.iter().rev().find(|line| line.contains(FAILED)) {
            // Say what QEMU's first console showed, which is also the proof
            // that the screendump path works while the card does not.
            let firmware = read_dump(&mut qmp, None, &dump)
                .map(|screen| {
                    format!(
                        "QEMU's first console was {}x{}",
                        screen.width, screen.height
                    )
                })
                .unwrap_or_else(|error| format!("no screendump either: {error}"));
            return Err(Error::new(format!("{arch}: {} ({firmware})", line.trim())));
        }
        let marker = lines
            .iter()
            .rev()
            .find(|line| line.contains(MARKER))
            .map_or("", |line| line.trim());
        let Some(plane) = primary_plane(marker) else {
            return Err(Error::new(format!(
                "{arch}: the program found no primary plane on the card: `{marker}`"
            )));
        };
        println!("  {arch}: the card's primary plane {plane} shows the framebuffer");
        if card_events(marker).as_deref() != Some(CARD_EVENTS) {
            return Err(Error::new(format!(
                "{arch}: the card's reads were not Linux's `drm_read`'s, \
                 `events {CARD_EVENTS}`: `{marker}`"
            )));
        }
        println!("  {arch}: the card's reads: events {CARD_EVENTS}");
        judge_render(arch, args.gl, args.venus, lines)?;
        // A GL console holds a texture on the host's GPU and no surface, and
        // QEMU's `screendump` reads only a surface (`docs/GPU.md` §3.1). So
        // the 3D boot is judged by what its render node did, above, and the
        // screen's pixels by the 2D boot, which drives the same display path.
        if args.gl {
            judged_without_a_dump = true;
            return Ok(());
        }
        let settle = Instant::now() + SETTLE;
        loop {
            let screen = read_dump(&mut qmp, Some(DEVICE_ID), &dump)?;
            let clean = mismatches(&screen, BACKGROUND, 0).1 == 0;
            if clean || Instant::now() >= settle {
                taken = Some(screen);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    if judged_without_a_dump {
        return Ok(None);
    }
    taken.map(Some).ok_or_else(|| {
        Error::new(format!(
            "{arch}: the program never printed `{MARKER}` within {}s",
            args.timeout
        ))
    })
}

/// `test-display` on each architecture asked for that has virtio-gpu.
pub(crate) fn test_display(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if target(arch).is_none() {
            println!("  {arch}: no virtio-gpu in QEMU's machine; skipped");
            continue;
        }
        // Every build before the first boot, so a plan run, which stops at
        // the first boot, records the negative control too.
        let plain = build_blank(arch, false)?;
        let negative = build_blank(arch, true)?;
        let Some(screen) = boot_and_dump(arch, &plain, args, "display")? else {
            println!(
                "  {arch}: a GL console cannot be dumped; the screen's pixels are the 2D boot's to judge"
            );
            continue;
        };
        let (found, count) = mismatches(&screen, BACKGROUND, 8);
        if count != 0 {
            return Err(Error::new(format!(
                "{arch}: {count} of {} pixels are not 0x1e1e2e; the first: {found:?}",
                screen.width * screen.height
            )));
        }
        println!(
            "  {arch}: all {} pixels of the {}x{} screen are 0x1e1e2e",
            screen.width * screen.height,
            screen.width,
            screen.height
        );

        let Some(screen) = boot_and_dump(arch, &negative, args, "display-negative")? else {
            continue;
        };
        let (found, count) = mismatches(&screen, BACKGROUND, 8);
        if count != 1 || found.first().map(|&(x, y, _)| (x, y)) != Some((0, 0)) {
            return Err(Error::new(format!(
                "{arch}: the negative control should differ at exactly pixel (0, 0); \
                 {count} pixels differ, the first: {found:?}"
            )));
        }
        println!("  {arch}: the negative control failed the check at exactly pixel (0, 0)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ppm(width: usize, height: usize, fill: [u8; 3]) -> Vec<u8> {
        let mut bytes = format!("P6\n# QEMU\n{width} {height}\n255\n").into_bytes();
        for _ in 0..width * height {
            bytes.extend_from_slice(&fill);
        }
        bytes
    }

    #[test]
    fn a_screendump_parses_and_is_judged_pixel_by_pixel() {
        let mut bytes = ppm(4, 3, BACKGROUND);
        let screen = parse_ppm(&bytes).expect("parses");
        assert_eq!(
            (screen.width, screen.height, screen.pixels.len()),
            (4, 3, 36)
        );
        assert_eq!(mismatches(&screen, BACKGROUND, 8), (vec![], 0));

        // Pixel (1, 2) wrong.
        let header = bytes.len() - 36;
        bytes[header + (2 * 4 + 1) * 3] = 0xFF;
        let screen = parse_ppm(&bytes).expect("parses");
        assert_eq!(
            mismatches(&screen, BACKGROUND, 8),
            (vec![(1, 2, [0xFF, 0x1E, 0x2E])], 1)
        );
    }

    #[test]
    fn a_malformed_screendump_is_refused() {
        let good = ppm(2, 2, BACKGROUND);
        assert!(parse_ppm(&good[..good.len() - 1]).is_err(), "short");
        assert!(parse_ppm(b"P5\n2 2\n255\n").is_err(), "greyscale");
        assert!(parse_ppm(b"P6\n2 2\n65535\n").is_err(), "wide samples");
        assert!(parse_ppm(b"P6\n2").is_err(), "no header");
        assert!(parse_ppm(b"P6\nx 2\n255\n").is_err(), "not a number");
    }

    #[test]
    fn the_marker_line_says_what_the_cards_reads_did() {
        let line = "compositor: scanout 1024x768 1024x768 colour 0x1e1e2e \
                    events EAGAIN 0 32 plane 4 Primary";
        assert_eq!(card_events(line).as_deref(), Some(CARD_EVENTS));
        assert_eq!(primary_plane(line), Some(4), "and still names its plane");
        assert_eq!(
            card_events("compositor: scanout 1024x768 events none flip 22 plane 4 Primary")
                .as_deref(),
            Some("none flip 22")
        );
        assert_eq!(card_events("compositor: scanout events EAGAIN 0"), None);
        assert_eq!(card_events("compositor: scanout plane 4 Primary"), None);
    }

    #[test]
    fn the_marker_line_must_name_a_primary_plane() {
        let line = "[  4.2] compositor: scanout 1024x768 1024x768 colour 0x1e1e2e plane 4 Primary";
        assert_eq!(primary_plane(line), Some(4));
        assert_eq!(
            primary_plane("compositor: scanout 1024x768 colour 0x1e1e2e plane 4 Overlay"),
            None
        );
        assert_eq!(
            primary_plane("compositor: scanout 1024x768 colour 0x1e1e2e plane none"),
            None
        );
        assert_eq!(
            primary_plane("compositor: scanout 1024x768 colour 0x1e1e2e"),
            None
        );
        assert_eq!(primary_plane("plane x Primary"), None);
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(
            json_string(r"C:\build\display.ppm"),
            r#""C:\\build\\display.ppm""#
        );
        assert_eq!(json_string("a\"b\n"), "\"a\\\"b\\u000a\"");
    }
}
