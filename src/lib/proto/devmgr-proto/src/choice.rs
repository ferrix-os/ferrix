//! Which GPU `devmgr` hands to `nvrm`: one, never two (`docs/NVIDIA.md`
//! §4.1, "One card").
//!
//! `nvrm` hosts NVIDIA's resource manager for the one device its START
//! carries, and says HELLO with `nvidiactl` and `nvidia0`. A second `nvrm`
//! on a second card would claim the same device files. And on nazuna, the
//! machine bare metal is to run on first, the second card (the RTX 3090 at
//! `03:00.0`) has never run `nvrm` at all. So `devmgr` hands over exactly
//! one GPU:
//!
//! * a boot option the program names (its word is the caller's: a library
//!   names no boot option, `check-build-flags.py`) can choose it by PCI
//!   address, `ssss:bb:dd.f` or `bb:dd.f`, in hexadecimal as `lspci`
//!   writes it;
//! * without it, the first GPU `devmgr` meets is the one: devices come in
//!   the kernel's enumeration order, bus by bus, so on nazuna that is the
//!   RTX 3060 at `01:00.0`.
//!
//! The choice is made once, at the first GPU met, whatever its hand-over
//! then decides: a chosen card that is refused (not isolated, say) does not
//! pass the choice on to the next one, which would start `nvrm` on a card
//! nobody chose. A value that is not an address chooses none, and says so,
//! so that a misspelt word starts nothing rather than the wrong card.
//!
//! Pure functions over the command line's bytes, tested on the host.

use core::fmt;

use crate::gpu::Place;

/// What the command line asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// No word: the first GPU met.
    First,
    /// The GPU at this PCI address word (`segment << 16 | bus << 8 |
    /// device << 3 | function`, as `DeviceInfo::location` has it).
    At(u32),
    /// A word whose value is not a PCI address: no GPU.
    Unparsed,
}

/// The word's value in `line`, the kernel command line: its first
/// whitespace-separated word that starts with `word` (`<name>=`).
#[must_use]
pub fn wanted(line: &[u8], word: &[u8]) -> Wanted {
    let Some(value) = line
        .split(u8::is_ascii_whitespace)
        .find_map(|token| token.strip_prefix(word))
    else {
        return Wanted::First;
    };
    address(value).map_or(Wanted::Unparsed, Wanted::At)
}

/// `ssss:bb:dd.f` or `bb:dd.f` as a PCI address word, or `None`.
#[must_use]
pub fn address(text: &[u8]) -> Option<u32> {
    let mut parts = text.split(|&byte| byte == b':');
    let (segment, bus, rest) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(segment), Some(bus), Some(rest), None) => (hex(segment, 4, 0xFFFF)?, bus, rest),
        (Some(bus), Some(rest), None, None) => (0, bus, rest),
        _ => return None,
    };
    let bus = hex(bus, 2, 0xFF)?;
    let mut rest = rest.split(|&byte| byte == b'.');
    let (Some(device), Some(function), None) = (rest.next(), rest.next(), rest.next()) else {
        return None;
    };
    let device = hex(device, 2, 0x1F)?;
    let function = hex(function, 1, 0x7)?;
    Some(segment << 16 | bus << 8 | device << 3 | function)
}

/// Hexadecimal digits, 1 to `width` of them, at most `max`.
fn hex(digits: &[u8], width: usize, max: u32) -> Option<u32> {
    if digits.is_empty() || digits.len() > width {
        return None;
    }
    let value = digits.iter().try_fold(0_u32, |value, &digit| {
        let nibble = char::from(digit).to_digit(16)?;
        Some(value << 4 | nibble)
    })?;
    (value <= max).then_some(value)
}

/// What [`Chooser::pick`] decided for one GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// This is the GPU: hand it over (the mark, isolation, the budget).
    Take {
        /// Whether the word chose it, or it was the first met.
        by_word: bool,
    },
    /// Not started: the word names another GPU.
    NotNamed {
        /// The address the word names.
        named: u32,
    },
    /// Not started: another GPU is the one.
    Second {
        /// The GPU chosen.
        chosen: u32,
    },
    /// Not started: the word's value is not an address.
    Unparsed,
}

impl Pick {
    /// Whether the GPU is to be handed over.
    #[must_use]
    pub const fn takes(self) -> bool {
        matches!(self, Pick::Take { .. })
    }
}

/// The one choice, made at the first GPU met and kept.
#[derive(Debug, Clone, Copy)]
pub struct Chooser {
    wanted: Wanted,
    chosen: Option<u32>,
    /// The boot option's name, for the lines: the caller's.
    name: &'static str,
}

impl Chooser {
    /// A chooser for what `wanted` read, the option being `name`.
    #[must_use]
    pub const fn new(wanted: Wanted, name: &'static str) -> Chooser {
        Chooser {
            wanted,
            chosen: None,
            name,
        }
    }

    /// Whether the GPU at `location` is the one. The same GPU asked again
    /// (its driver started anew) is taken again.
    pub fn pick(&mut self, location: u32) -> Pick {
        match self.wanted {
            Wanted::Unparsed => Pick::Unparsed,
            Wanted::At(named) if named != location => Pick::NotNamed { named },
            Wanted::At(_) => {
                self.chosen = Some(location);
                Pick::Take { by_word: true }
            }
            Wanted::First => match self.chosen {
                Some(chosen) if chosen != location => Pick::Second { chosen },
                _ => {
                    self.chosen = Some(location);
                    Pick::Take { by_word: false }
                }
            },
        }
    }

    /// The line `devmgr` says `pick` for the GPU at `location` with.
    #[must_use]
    pub const fn line(&self, pick: Pick, location: u32) -> PickLine {
        PickLine {
            pick,
            location,
            name: self.name,
        }
    }
}

/// [`Chooser::line`]'s text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickLine {
    pick: Pick,
    location: u32,
    name: &'static str,
}

impl fmt::Display for PickLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = Place(self.location);
        let name = self.name;
        match self.pick {
            Pick::Take { by_word: true } => {
                write!(
                    f,
                    "devmgr   gpu {place}: the one nvrm drives, as {name} names it"
                )
            }
            Pick::Take { by_word: false } => write!(
                f,
                "devmgr   gpu {place}: the one nvrm drives, the first GPU met ({name}=<address> \
                 chooses another)"
            ),
            Pick::NotNamed { named } => write!(
                f,
                "devmgr   gpu {place} not started: {name} names {}",
                Place(named)
            ),
            Pick::Second { chosen } => write!(
                f,
                "devmgr   gpu {place} not started: nvrm drives one GPU, {} ({name}=<address> \
                 chooses another)",
                Place(chosen)
            ),
            Pick::Unparsed => write!(
                f,
                "devmgr   gpu {place} not started: {name}'s value is not a PCI address \
                 (ssss:bb:dd.f or bb:dd.f), so no GPU is chosen"
            ),
        }
    }
}
