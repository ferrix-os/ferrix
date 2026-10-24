//! Which GPU `devmgr` hands to `nvrm` (`crate::choice`): one, never two.

extern crate std;

use std::format;

use crate::choice::{self, Chooser, Pick, Wanted};

/// The word as `devmgr` spells it, here only so the lines read as they do
/// on a console.
const WORD: &[u8] = b"ferrix.nvidia=";
const NAME: &str = "ferrix.nvidia";

/// nazuna's two cards, as `DeviceInfo::location` has them.
const RTX_3060: u32 = 0x01 << 8;
const RTX_3090: u32 = 0x03 << 8;

#[test]
fn addresses_are_read_as_lspci_writes_them() {
    assert_eq!(choice::address(b"01:00.0"), Some(RTX_3060));
    assert_eq!(choice::address(b"0000:03:00.0"), Some(RTX_3090));
    assert_eq!(
        choice::address(b"0001:1f:1f.7"),
        Some(1 << 16 | 0x1F << 8 | 0x1F << 3 | 7)
    );
    assert_eq!(choice::address(b"3:0.0"), Some(RTX_3090));
    assert_eq!(choice::address(b"0A:00.1"), Some(0x0A << 8 | 1));
}

#[test]
fn what_is_not_an_address_is_none() {
    for text in [
        &b""[..],
        b"01",
        b"01:00",
        b"01:00.",
        b"01:20.0",
        b"01:00.8",
        b"100:00.0",
        b"10000:01:00.0",
        b"0:0:01:00.0",
        b"01:00.0.0",
        b"xx:00.0",
        b"01:00.0 ",
        b"-1:00.0",
    ] {
        assert_eq!(choice::address(text), None, "{text:?}");
    }
}

#[test]
fn the_word_is_found_among_the_others() {
    assert_eq!(
        choice::wanted(b"ferrix.root=btrfs ferrix.fbcon", WORD),
        Wanted::First
    );
    assert_eq!(choice::wanted(b"", WORD), Wanted::First);
    assert_eq!(
        choice::wanted(
            b"ferrix.root=btrfs ferrix.nvidia=0000:01:00.0 ferrix.fbcon\n",
            WORD
        ),
        Wanted::At(RTX_3060)
    );
    assert_eq!(
        choice::wanted(b"ferrix.nvidia=03:00.0", WORD),
        Wanted::At(RTX_3090)
    );
    assert_eq!(
        choice::wanted(b"ferrix.nvidia=3090", WORD),
        Wanted::Unparsed
    );
    assert_eq!(choice::wanted(b"ferrix.nvidia=", WORD), Wanted::Unparsed);
    // A longer word that only starts the same is not it.
    assert_eq!(
        choice::wanted(b"ferrix.nvidiax=01:00.0", WORD),
        Wanted::First
    );
    // The first word counts.
    assert_eq!(
        choice::wanted(b"ferrix.nvidia=01:00.0 ferrix.nvidia=03:00.0", WORD),
        Wanted::At(RTX_3060)
    );
}

#[test]
fn without_the_word_the_first_gpu_met_is_the_one() {
    let mut chooser = Chooser::new(Wanted::First, NAME);
    assert_eq!(chooser.pick(RTX_3060), Pick::Take { by_word: false });
    assert_eq!(chooser.pick(RTX_3090), Pick::Second { chosen: RTX_3060 });
    // Its driver started again: the same card is taken again.
    assert_eq!(chooser.pick(RTX_3060), Pick::Take { by_word: false });
    assert_eq!(chooser.pick(RTX_3090), Pick::Second { chosen: RTX_3060 });
}

#[test]
fn a_refused_first_card_does_not_pass_the_choice_on() {
    // The first GPU is chosen when met, whether its hand-over then starts
    // nvrm or not: the next card is never started in its place.
    let mut chooser = Chooser::new(Wanted::First, NAME);
    assert!(chooser.pick(RTX_3060).takes());
    assert!(!chooser.pick(RTX_3090).takes());
}

#[test]
fn the_word_chooses_the_card_it_names_and_no_other() {
    let mut chooser = Chooser::new(Wanted::At(RTX_3090), NAME);
    assert_eq!(chooser.pick(RTX_3060), Pick::NotNamed { named: RTX_3090 });
    assert_eq!(chooser.pick(RTX_3090), Pick::Take { by_word: true });
    assert_eq!(chooser.pick(RTX_3090), Pick::Take { by_word: true });

    // A card it names that is not there: none is started.
    let mut chooser = Chooser::new(Wanted::At(0x05 << 8), NAME);
    assert!(!chooser.pick(RTX_3060).takes());
    assert!(!chooser.pick(RTX_3090).takes());
}

#[test]
fn a_misspelt_value_chooses_none() {
    let mut chooser = Chooser::new(Wanted::Unparsed, NAME);
    assert_eq!(chooser.pick(RTX_3060), Pick::Unparsed);
    assert_eq!(chooser.pick(RTX_3090), Pick::Unparsed);
}

#[test]
fn every_pick_has_its_line() {
    let chooser = Chooser::new(Wanted::First, NAME);
    let line = |pick, location| format!("{}", chooser.line(pick, location));
    assert_eq!(
        line(Pick::Take { by_word: false }, RTX_3060),
        "devmgr   gpu 01:00.0: the one nvrm drives, the first GPU met (ferrix.nvidia=<address> \
         chooses another)"
    );
    assert_eq!(
        line(Pick::Take { by_word: true }, RTX_3090),
        "devmgr   gpu 03:00.0: the one nvrm drives, as ferrix.nvidia names it"
    );
    assert_eq!(
        line(Pick::NotNamed { named: RTX_3090 }, RTX_3060),
        "devmgr   gpu 01:00.0 not started: ferrix.nvidia names 03:00.0"
    );
    assert_eq!(
        line(Pick::Second { chosen: RTX_3060 }, RTX_3090),
        "devmgr   gpu 03:00.0 not started: nvrm drives one GPU, 01:00.0 \
         (ferrix.nvidia=<address> chooses another)"
    );
    assert_eq!(
        line(Pick::Unparsed, RTX_3060),
        "devmgr   gpu 01:00.0 not started: ferrix.nvidia's value is not a PCI address \
         (ssss:bb:dd.f or bb:dd.f), so no GPU is chosen"
    );
}
