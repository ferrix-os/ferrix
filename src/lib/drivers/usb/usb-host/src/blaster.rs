//! JTAG through an Altera USB-Blaster: the bytes its CPLD takes, behind
//! the FT245B FIFO [`crate::ftdi`] drives, and the scans made of them.
//!
//! The protocol is the one Kolja Waschk's USB-JTAG project read off the
//! cable, which `OpenOCD`'s `usb_blaster` driver and `UrJTAG` speak. Every byte
//! the host sends is one of two kinds:
//!
//! * **Bit-bang** (bit 7 clear): the byte sets the pins -- [`TCK`] in bit
//!   0, [`TMS`] in 1, nCE and nCS in 2 and 3 (held high in JTAG mode),
//!   [`TDI`] in 4, the LED in 5 -- and with [`READ`], bit 6, the cable
//!   answers one byte whose bit 0 is TDO as it is now. A TCK pulse is two
//!   bytes: TCK low, then TCK high with the same TMS and TDI.
//! * **Byte-shift** (bit 7 set): bits 5..0 count the bytes that follow, up
//!   to 63, which are clocked out on TDI least significant bit first with
//!   TMS low; with [`READ`] set the cable answers as many bytes of TDO, the
//!   same way round. TCK must be low before, and a header and its bytes
//!   must lie in one 64-byte USB packet, as `OpenOCD` keeps them.
//!
//! A scan's last bit has to leave Shift-IR or Shift-DR with TMS high, which
//! byte-shift cannot do, so the last one to eight bits go bit-banged.
//!
//! [`Jtag`] keeps the state machine in Run-Test/Idle between scans, and
//! knows no more about the chain than [`Jtag::idcodes`] reads from it.
//! It is written against [`Link`], which the bus gives for a cable it
//! found (`crate::bus::Bus::blaster`) and the tests give for a model.

/// Bit-bang: TCK.
pub const TCK: u8 = 1 << 0;
/// Bit-bang: TMS.
pub const TMS: u8 = 1 << 1;
/// Bit-bang: nCE, held high in JTAG mode.
pub const NCE: u8 = 1 << 2;
/// Bit-bang: nCS, held high in JTAG mode.
pub const NCS: u8 = 1 << 3;
/// Bit-bang: TDI.
pub const TDI: u8 = 1 << 4;
/// Bit-bang: the output enable, and the cable's LED.
pub const LED: u8 = 1 << 5;
/// Either kind: answer with TDO.
pub const READ: u8 = 1 << 6;
/// Byte-shift: the header's mark.
pub const SHIFT: u8 = 1 << 7;
/// Byte-shift: the most bytes one header counts.
pub const SHIFT_MAX: usize = 63;

/// The USB packet a header and its bytes must lie in.
const PACKET: usize = 64;
/// Bytes held before they are sent: a bulk OUT transfer's worth.
const QUEUE: usize = 4096;

/// The most taps [`Jtag::idcodes`] reads.
pub const MAX_TAPS: usize = 8;
/// An IDCODE of all ones: what TDI's own ones read as once every tap's
/// register is through, so the end of the chain.
const END_OF_CHAIN: u32 = u32::MAX;

/// Where the bytes go and come from: the cable's bulk pipes, with the
/// FTDI status bytes already gone.
pub trait Link {
    /// What a transfer fails with.
    type Error;
    /// Send `bytes`, all of them.
    ///
    /// # Errors
    ///
    /// The transfer's.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
    /// Fill `into` with the next bytes the cable sends, waiting for them.
    ///
    /// # Errors
    ///
    /// The transfer's, or a cable that sent too few in time.
    fn read(&mut self, into: &mut [u8]) -> Result<(), Self::Error>;
}

/// Why a scan failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error<E> {
    /// The link's.
    Link(E),
    /// More bits than the buffers hold.
    TooLong,
    /// The chain read made no sense: no end of it within [`MAX_TAPS`]
    /// taps, or TDO stuck low.
    Chain,
}

/// One tap of a chain, as an IDCODE scan finds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tap {
    /// One that put its IDCODE in DR at reset.
    Idcode(u32),
    /// One that put BYPASS there: a single zero.
    Bypass,
}

/// The JTAG state machine behind a cable, kept in Run-Test/Idle between
/// scans.
#[derive(Debug)]
pub struct Jtag<'a, L: Link> {
    link: &'a mut L,
    queue: [u8; QUEUE],
    queued: usize,
    tms: bool,
    tdi: bool,
}

impl<'a, L: Link> Jtag<'a, L> {
    /// The machine behind `link`, in whatever state it is: [`Jtag::reset`]
    /// says.
    pub const fn new(link: &'a mut L) -> Self {
        Jtag {
            link,
            queue: [0; QUEUE],
            queued: 0,
            tms: false,
            tdi: false,
        }
    }

    // -----------------------------------------------------------------------
    // The bytes
    // -----------------------------------------------------------------------

    /// A bit-bang byte for the pins as they are, TCK low.
    const fn pins(&self) -> u8 {
        let mut byte = LED | NCE | NCS;
        if self.tms {
            byte |= TMS;
        }
        if self.tdi {
            byte |= TDI;
        }
        byte
    }

    fn flush(&mut self) -> Result<(), Error<L::Error>> {
        if self.queued > 0 {
            let bytes = self.queue.get(..self.queued).unwrap_or(&[]);
            self.link.write(bytes).map_err(Error::Link)?;
            self.queued = 0;
        }
        Ok(())
    }

    fn put(&mut self, byte: u8) -> Result<(), Error<L::Error>> {
        if self.queued == QUEUE {
            self.flush()?;
        }
        if let Some(slot) = self.queue.get_mut(self.queued) {
            *slot = byte;
            self.queued += 1;
        }
        Ok(())
    }

    /// Read `into.len()` answer bytes, after sending what is queued.
    fn answer(&mut self, into: &mut [u8]) -> Result<(), Error<L::Error>> {
        self.flush()?;
        if into.is_empty() {
            return Ok(());
        }
        self.link.read(into).map_err(Error::Link)
    }

    /// One TCK pulse with TMS as given and TDI low.
    fn clock_tms(&mut self, tms: bool) -> Result<(), Error<L::Error>> {
        self.tms = tms;
        self.tdi = false;
        let out = self.pins();
        self.put(out)?;
        self.put(out | TCK)
    }

    /// TCK back low, the pins as they are.
    fn idle_clock(&mut self) -> Result<(), Error<L::Error>> {
        let out = self.pins();
        self.put(out)
    }

    /// TMS pulses, the first bit of `bits` first.
    fn tms_sequence(&mut self, bits: &[bool]) -> Result<(), Error<L::Error>> {
        for &bit in bits {
            self.clock_tms(bit)?;
        }
        self.idle_clock()
    }

    // -----------------------------------------------------------------------
    // The machine
    // -----------------------------------------------------------------------

    /// Five TCKs with TMS high, which take any state to Test-Logic-Reset,
    /// then one low to Run-Test/Idle. Every tap's IR then holds IDCODE, or
    /// BYPASS for a tap with no IDCODE.
    ///
    /// # Errors
    ///
    /// The link's.
    pub fn reset(&mut self) -> Result<(), Error<L::Error>> {
        self.tms_sequence(&[true, true, true, true, true, false])?;
        self.flush()
    }

    /// Shift `bits` bits of `tdi` (least significant bit of byte 0 first)
    /// through IR, or DR, from Run-Test/Idle and back to it, and what came
    /// out of TDO into `tdo` the same way round.
    ///
    /// # Errors
    ///
    /// The link's; [`Error::TooLong`] for buffers shorter than `bits`.
    pub fn scan(
        &mut self,
        ir: bool,
        bits: usize,
        tdi: &[u8],
        tdo: &mut [u8],
    ) -> Result<(), Error<L::Error>> {
        let bytes = bits.div_ceil(8);
        if bits == 0 || tdi.len() < bytes || tdo.len() < bytes {
            return Err(Error::TooLong);
        }
        // Select-DR-Scan, (Select-IR-Scan,) Capture, Shift.
        if ir {
            self.tms_sequence(&[true, true, false, false])?;
        } else {
            self.tms_sequence(&[true, false, false])?;
        }
        self.shift(bits, tdi, tdo)?;
        // Exit1 to Update, then Run-Test/Idle.
        self.tms_sequence(&[true, false])?;
        self.flush()
    }

    /// The shift itself, from Shift-IR or Shift-DR with TCK low, to Exit1.
    fn shift(&mut self, bits: usize, tdi: &[u8], tdo: &mut [u8]) -> Result<(), Error<L::Error>> {
        // The last bit is bit-banged, so a whole number of bytes leaves its
        // last byte to bit-bang.
        let mut whole = bits / 8;
        let mut rest = bits % 8;
        if whole > 0 && rest == 0 {
            whole -= 1;
            rest = 8;
        }
        self.tms = false;
        let mut done = 0;
        while done < whole {
            let room = PACKET - self.queued % PACKET;
            if room < 2 {
                // Not even a header and one byte: start the next packet.
                for _ in 0..room {
                    let out = self.pins();
                    self.put(out)?;
                }
                continue;
            }
            let count = (room - 1).min(whole - done).min(SHIFT_MAX);
            self.put(SHIFT | READ | count as u8)?;
            for at in done..done + count {
                let byte = tdi.get(at).copied().unwrap_or(0);
                self.put(byte)?;
            }
            let into = tdo.get_mut(done..done + count).ok_or(Error::TooLong)?;
            self.answer(into)?;
            done += count;
        }
        let mut answers = [0_u8; 8];
        for bit in 0..rest {
            let index = whole * 8 + bit;
            self.tdi = tdi.get(index / 8).is_some_and(|byte| byte >> (index % 8) & 1 != 0);
            let last = bit + 1 == rest;
            if last {
                self.tms = true;
            }
            let out = self.pins();
            self.put(out)?;
            self.put(out | TCK | READ)?;
            if last {
                self.put(out)?;
            }
        }
        let answers = answers.get_mut(..rest).ok_or(Error::TooLong)?;
        self.answer(answers)?;
        let last = tdo.get_mut(whole).ok_or(Error::TooLong)?;
        *last = 0;
        for (bit, answer) in answers.iter().enumerate() {
            if answer & 1 != 0 {
                *last |= 1 << bit;
            }
        }
        self.tdi = false;
        Ok(())
    }

    /// Reset the chain and read what each tap put in DR: its IDCODE, or a
    /// single zero for BYPASS, the tap nearest TDO first, into `taps`. How
    /// many there are.
    ///
    /// # Errors
    ///
    /// The link's; [`Error::Chain`] when the chain does not end within
    /// [`MAX_TAPS`] taps or TDO reads stuck low.
    pub fn idcodes(&mut self, taps: &mut [Tap; MAX_TAPS]) -> Result<usize, Error<L::Error>> {
        const BITS: usize = 32 * (MAX_TAPS + 1);
        self.reset()?;
        let ones = [0xFF_u8; BITS / 8];
        let mut out = [0_u8; BITS / 8];
        self.scan(false, BITS, &ones, &mut out)?;
        let bit = |at: usize| out.get(at / 8).is_some_and(|byte| byte >> (at % 8) & 1 != 0);
        let word = |at: usize| (0..32).fold(0_u32, |word, n| word | (u32::from(bit(at + n)) << n));
        let mut at = 0;
        for (found, slot) in taps.iter_mut().enumerate() {
            if at + 32 > BITS {
                return Err(Error::Chain);
            }
            if !bit(at) {
                *slot = Tap::Bypass;
                at += 1;
                // A run of zeros as long as a register is TDO held low,
                // not a chain of bypassed taps.
                if (0..32).all(|n| !bit(at + n)) {
                    return Err(Error::Chain);
                }
                continue;
            }
            let idcode = word(at);
            if idcode == END_OF_CHAIN {
                return Ok(found);
            }
            *slot = Tap::Idcode(idcode);
            at += 32;
        }
        if word(at) == END_OF_CHAIN {
            Ok(MAX_TAPS)
        } else {
            Err(Error::Chain)
        }
    }
}

/// An IDCODE's fields: version, part number, JEDEC manufacturer.
#[must_use]
pub const fn fields(idcode: u32) -> (u8, u16, u16) {
    (
        (idcode >> 28) as u8,
        (idcode >> 12) as u16,
        ((idcode >> 1) & 0x7FF) as u16,
    )
}
