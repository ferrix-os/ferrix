//! `cargo xtask test-ft232h`: the customer's FT232H, the real chip, passed
//! through to a guest (`docs/USB-SERIAL.md` §3).
//!
//! The adapter is the host's: `0403:6014` on bus 1, which the customer
//! approved for this, and nothing else of the host's is touched. QEMU's
//! `usb-host` takes it by bus, vendor and product, detaches the host's
//! `ftdi_sio` from it alone, and gives it back when QEMU exits; the gate
//! checks the host has its `/dev/ttyUSB0` again afterwards.
//!
//! The chip's TX and RX are wired to nothing, so what the gate can hold a
//! real chip to is what the chip itself answers:
//!
//! 1. enumeration: `usbhid` exports it, devmgr starts `usbserial`, which
//!    names it an FT232H with 512-byte packets and its serial number;
//! 2. the H divisor and the line: `stty` at 9600, 115200, 1000000 and
//!    3000000 baud, and 7 data bits, odd parity and two stop bits, each
//!    taken by the chip (a refused request is said, and fails the gate),
//!    the rate the chip runs at said beside it;
//! 3. the modem lines, as the chip's status bytes give them;
//! 4. data both ways without wires: the MPSSE loopback, 4096 bytes out
//!    through the engine and back, compared, then the chip a UART again.
//!
//! What is not shown here: bytes on the pins, and the rate on the wire,
//! which need a second device or an oscilloscope.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-ft232h",
    help_at: 486,
    help: &[
        "test-ft232h   Pass the host's FT232H (0403:6014, bus 1) to a guest: its enumeration, the H divisor",
        "              at four rates, the line settings, the modem lines and an MPSSE loopback (x86_64)",
    ],
    run: |_, args| test(args),
}];

/// The adapter, as QEMU's `usb-host` picks it.
const DEVICE: &str = "hostbus=1,vendorid=0x0403,productid=0x6014";
/// The host's node for it, which must be back once QEMU has let go.
const HOST_NODE: &str = "/dev/ttyUSB0";
/// What the script prints once it has done everything.
const DONE: &str = "hw: done";
/// The rates set, each a `B` code `stty` knows.
const RATES: [u32; 4] = [9600, 115_200, 1_000_000, 3_000_000];

fn script() -> String {
    let rates: Vec<String> = RATES.iter().map(u32::to_string).collect();
    format!(
        "i=0\n\
         while [ ! -e /tmp/ttyUSB0 ]; do i=$((i+1)); \
         if [ $i -gt 300 ]; then echo 'hw: failed: no /tmp/ttyUSB0'; exit 1; fi; sleep 0.1; done\n\
         echo 'hw: ready'\n\
         for r in {rates}; do stty -F /tmp/ttyUSB0 $r raw -echo -crtscts && echo \"hw: set $r\"; sleep 0.3; done\n\
         stty -F /tmp/ttyUSB0 115200 cs7 parenb parodd cstopb && echo 'hw: framing'\n\
         sleep 0.3\n\
         stty -F /tmp/ttyUSB0 cs8 -parenb -parodd -cstopb\n\
         sleep 0.3\n\
         echo hello-from-ferrix > /tmp/ttyUSB0 && echo 'hw: wrote'\n\
         touch /tmp/usbserial-loopback-0\n\
         sleep 3\n\
         echo '{DONE}'\n\
         sleep 2\n",
        rates = rates.join(" "),
    )
}

/// `test-ft232h`.
///
/// # Errors
///
/// A boot that did not show each of the module documentation's lines, or
/// a host that did not get its adapter back.
pub(crate) fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if !Path::new("/dev/bus/usb/001").is_dir() {
        return Err(Error::new("no USB bus 1 on this host"));
    }
    let shell = crate::usb_serial::busybox(arch, args)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel_with_init(arch, args.release, &shell, &script())?;
    let natives = crate::native::build(arch, args.release)?;
    let initramfs = crate::initramfs::build(Some(&shell), &natives, None, &[])?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
    let mut booted = args.clone();
    booted.usb_host = Some(DEVICE.to_owned());
    let lines = crate::qemu::watch_then(arch, &image, &kernel, &booted, DONE, |watching| {
        watching.stop_when_done();
        Ok(())
    })?;
    let mut failures = Vec::new();
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    let mut wanted: Vec<String> = vec![
        "usbhid: interface 0 of 0403:6014 exported for its class driver".to_owned(),
        "devmgr   usbserial for usb 0403:6014 interface 0: started (serial 0)".to_owned(),
        "usbserial: serial 0 0403:6014 (FT232H UART, 512-byte packets, serial ".to_owned(),
        "usbserial: serial 0 modem lines: ".to_owned(),
        "usbserial: serial 0 set to 115200 baud (115246 on the chip), 7 data bits, parity Odd, 2 stop bits".to_owned(),
        "hw: wrote".to_owned(),
        "usbserial: serial 0 loopback through the chip's MPSSE: 4096 bytes back as sent".to_owned(),
    ];
    for rate in RATES {
        wanted.push(format!("hw: set {rate}"));
        let actual = crate::usb_serial::h_rate(rate);
        wanted.push(format!(
            "usbserial: serial 0 set to {rate} baud ({actual} on the chip), 8 data bits"
        ));
    }
    for want in &wanted {
        if !has(want) {
            failures.push(format!("never said: `{want}`"));
        }
    }
    for refused in ["cannot be set", "would not", "failed", "dropped", "lost "] {
        if let Some(line) = lines
            .iter()
            .find(|line| line.contains("usbserial") && line.contains(refused))
        {
            failures.push(format!("the driver said `{}`", line.trim()));
        }
    }
    // QEMU has let go: the host's ftdi_sio takes the adapter back.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !Path::new(HOST_NODE).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    if !Path::new(HOST_NODE).exists() {
        failures.push(format!(
            "{HOST_NODE} did not come back on the host: the adapter is still detached"
        ));
    }
    if !failures.is_empty() {
        let mut message = format!("{arch}: the FT232H did not answer as it should:\n");
        for failure in &failures {
            message.push_str("    - ");
            message.push_str(failure);
            message.push('\n');
        }
        return Err(Error::new(message));
    }
    for line in lines.iter().filter(|line| line.contains("usbserial: ")) {
        println!("  {arch}: {}", line.trim());
    }
    println!(
        "  {arch}: the real FT232H enumerated, took every rate and the framing, gave its modem \
         lines, looped 4096 bytes through its MPSSE, and went back to the host's {HOST_NODE}"
    );
    Ok(())
}
