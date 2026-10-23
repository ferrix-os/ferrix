//! What the compositor was asked to do.

use std::path::PathBuf;

/// The compositor's arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The configuration file, or `None` for the built-in defaults.
    pub config: Option<PathBuf>,
    /// The Wayland socket's name, as `WAYLAND_DISPLAY` will carry it.
    pub display: String,
    /// Draw into memory rather than onto a screen, at this size.
    pub headless: Option<(u32, u32)>,
    /// Stop after this many frames. `None` runs until it is killed, which is
    /// what a compositor does.
    pub frames: Option<u32>,
    /// Write each frame here as a PPM, for a test to look at.
    pub dump: Option<PathBuf>,
    /// Programs to start once the socket is listening, beside the
    /// configuration's own `exec-once`.
    pub exec: Vec<String>,
    /// Give up after this many milliseconds, so a test can never hang.
    pub deadline: Option<u64>,
    /// lode's screenshot-provider socket and the session's token, when
    /// `sessiond` gave this compositor one (`crate::lodeshot`); `main` fills
    /// it from the environment, a test with a socket of its own.
    pub lode: Option<(PathBuf, [u8; 16])>,
    /// The instance name `hyprctl` finds the control socket under, in
    /// `$XDG_RUNTIME_DIR/hypr/<instance>/`. Without one the socket is not
    /// bound at all, which is what a test that does not want it asks for.
    pub instance: Option<String>,
    /// What draws the frame.
    pub renderer: Renderer,
    /// How many of its first frames a headless screen refuses to show, as
    /// a card whose driver timed out does: what a test of the compositor
    /// riding a stalled driver out asks for. Not a command-line option.
    pub refuse_flips: u32,
    /// Move every animation on by this many milliseconds a drawn frame
    /// rather than by the wall clock's: what a test of an animation's
    /// frames asks for, so that a slide is the same frames however busy
    /// the host is. A two-second slide drawn by a starved compositor was
    /// three frames on one run and twenty on the next, and a test that
    /// counts the places between was a test of the host's load. `None` is
    /// the wall clock, which is what a screen wants. Not a command-line
    /// option.
    pub animation_step: Option<u32>,
    /// Where the arrangement of each set of monitors is saved
    /// (`crate::layouts`): `$XDG_STATE_HOME/hyprix` when started from the
    /// command line, nowhere for a compositor made in a test.
    pub state_dir: Option<PathBuf>,
}

/// What draws the frame: `--renderer`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Renderer {
    /// The GPU when the card has one behind it and its render node speaks
    /// virgl, and the software renderer otherwise. What a person wants.
    #[default]
    Auto,
    /// The software renderer, whatever the card has.
    Software,
    /// The GPU, and it is an error for there to be none: what a test of the
    /// GPU's frames asks for, so that a missing GPU is not a pass.
    Gpu,
    /// virglrenderer's test server on this host, started by the compositor:
    /// the GPU's frames with no guest, which is how they are developed.
    Vtest,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config: None,
            display: "wayland-1".to_owned(),
            headless: None,
            frames: None,
            dump: None,
            exec: Vec::new(),
            deadline: None,
            lode: None,
            instance: None,
            renderer: Renderer::Auto,
            refuse_flips: 0,
            animation_step: None,
            state_dir: None,
        }
    }
}

impl Options {
    /// What to print when the arguments are wrong.
    pub const USAGE: &'static str = "\
usage: hyprix [options]

  --config <path>     a hyprland.conf; without one, Hyprland's defaults
  --display <name>    the Wayland socket's name (default wayland-1)
  --headless <WxH>    draw into memory at this size instead of onto a screen
  --frames <n>        stop after this many frames
  --dump <dir>        write each frame there as a PPM
  --exec <command>    start this once the socket is listening, repeatable
  --deadline <ms>     give up after this long
  --instance <name>   bind hyprctl's socket under $XDG_RUNTIME_DIR/hypr/<name>
  --renderer <which>  auto, software, gpu or vtest (default auto)";

    /// Read the arguments.
    ///
    /// # Errors
    ///
    /// A message naming the argument that was wrong.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut options = Self {
            state_dir: crate::layouts::default_dir(),
            ..Self::default()
        };
        let mut args = compositor_evecho::init::unshell(args.into_iter().collect()).into_iter();
        while let Some(argument) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{argument} needs a value"))
            };
            match argument.as_str() {
                "--config" => options.config = Some(PathBuf::from(value()?)),
                "--display" => options.display = value()?,
                "--headless" => options.headless = Some(size(&value()?)?),
                "--frames" => {
                    options.frames = Some(
                        value()?
                            .parse()
                            .map_err(|_| "--frames takes a number".to_owned())?,
                    );
                }
                "--dump" => options.dump = Some(PathBuf::from(value()?)),
                "--instance" => options.instance = Some(value()?),
                "--exec" => options.exec.push(value()?),
                "--deadline" => {
                    options.deadline = Some(
                        value()?
                            .parse()
                            .map_err(|_| "--deadline takes milliseconds".to_owned())?,
                    );
                }
                "--renderer" => {
                    options.renderer = match value()?.as_str() {
                        "auto" => Renderer::Auto,
                        "software" => Renderer::Software,
                        "gpu" => Renderer::Gpu,
                        "vtest" => Renderer::Vtest,
                        other => {
                            return Err(format!(
                                "--renderer takes auto, software, gpu or vtest, not {other}"
                            ));
                        }
                    };
                }
                "--help" | "-h" => return Err("help".to_owned()),
                other => return Err(format!("{other} is not an option")),
            }
        }
        Ok(options)
    }
}

/// `WxH`, as `--headless 1024x768` gives it.
fn size(text: &str) -> Result<(u32, u32), String> {
    let (width, height) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("{text} is not a size like 1024x768"))?;
    let read = |value: &str, what: &str| {
        value
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(|| format!("{what} of a size must be a number above zero"))
    };
    Ok((read(width, "the width")?, read(height, "the height")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        Options::parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn the_defaults_are_a_compositor_on_a_screen() {
        let options = parse(&[]).expect("no arguments is valid");
        assert_eq!(options.display, "wayland-1");
        assert_eq!(options.headless, None, "a screen by default");
        assert_eq!(options.frames, None, "until it is killed");
        assert!(options.exec.is_empty());
    }

    #[test]
    fn every_option_is_read() {
        let options = parse(&[
            "--config",
            "/etc/hyprland.conf",
            "--display",
            "wayland-9",
            "--headless",
            "1024x768",
            "--frames",
            "3",
            "--dump",
            "/tmp/frames",
            "--exec",
            "one",
            "--exec",
            "two",
            "--deadline",
            "5000",
        ])
        .expect("valid");
        assert_eq!(
            options.config.as_deref(),
            Some(std::path::Path::new("/etc/hyprland.conf"))
        );
        assert_eq!(options.display, "wayland-9");
        assert_eq!(options.headless, Some((1024, 768)));
        assert_eq!(options.frames, Some(3));
        assert_eq!(options.exec, ["one", "two"]);
        assert_eq!(options.deadline, Some(5000));
        assert_eq!(
            parse(&["--instance", "ferrix"])
                .expect("valid")
                .instance
                .as_deref(),
            Some("ferrix")
        );
    }

    #[test]
    fn a_size_must_be_two_numbers_above_zero() {
        for bad in ["1024", "1024x", "x768", "0x768", "1024x0", "-1x2", "axb"] {
            assert!(
                parse(&["--headless", bad]).is_err(),
                "{bad} was accepted as a size"
            );
        }
        assert_eq!(
            parse(&["--headless", "16X9"]).expect("valid").headless,
            Some((16, 9)),
            "a capital X is a size too"
        );
    }

    #[test]
    fn the_arguments_a_kernel_gives_its_first_program_are_read() {
        // `src/kernel/src/init.rs` starts the first program as `sh -i` or as
        // `sh -c <script>`, and the compositor is that program when it runs
        // as init.
        assert_eq!(
            parse(&["-i"]).expect("valid"),
            Options {
                state_dir: crate::layouts::default_dir(),
                ..Options::default()
            }
        );
        let options = parse(&["-c", "--headless 800x600 --frames 2"]).expect("valid");
        assert_eq!(options.headless, Some((800, 600)));
        assert_eq!(options.frames, Some(2));
        // An argument that holds a space needs a line of its own, since a
        // script has no quoting.
        let options =
            parse(&["-c", "--exec\n/bin/pattern checkerboard one\n--frames\n1"]).expect("valid");
        assert_eq!(options.exec, ["/bin/pattern checkerboard one"]);
        assert_eq!(options.frames, Some(1));
        // `-i` is only the shell's when it is the whole command line; a
        // compositor given it beside real options is a mistake worth saying.
        assert!(parse(&["-i", "--frames", "1"]).is_err());
    }

    #[test]
    fn an_option_without_its_value_and_an_unknown_one_are_refused() {
        assert!(parse(&["--display"]).is_err());
        assert!(parse(&["--frames", "many"]).is_err());
        assert!(parse(&["--nonsense"]).is_err());
    }
}
