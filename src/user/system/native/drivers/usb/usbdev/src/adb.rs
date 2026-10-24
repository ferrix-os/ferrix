//! adb over USB: the adb interface's two bulk endpoints bridged to a Unix
//! socket that `adbd` connects to (`docs/ADB.md` §4, step 3).
//!
//! The shape is `vport`'s: the driver moves bytes and understands as little
//! of them as it can, and the Linux program on the other side of the socket
//! speaks the protocol. Two things adb over USB needs that a byte stream
//! does not:
//!
//! * **A transfer per part.** The host reads a message's 24-byte header with
//!   one bulk read and its payload with another, as AOSP's adbd sends them,
//!   so each goes out as a transfer of its own: a transfer that ran on into
//!   the payload would overrun the host's header read. So the bridge reads
//!   the header's length word and hands the controller one part at a time,
//!   the next only once the last has gone.
//! * **A payload that fits one transfer.** adbd over USB offers at most
//!   [`MAX_PAYLOAD`] bytes, the endpoint's ring, so a whole payload is one
//!   transfer.
//!
//! A stream that stops parsing as adb, a USB reset or a disconnect drops the
//! client: adbd connects again and starts clean, and the host sends a new
//! `CNXN` when it comes back.

use ferrix_dwc3::usb_device::acm::ADB_IN;
use ferrix_linux_abi::errno::Errno;
use ferrix_rt::linux;

use crate::{Step, Usb};

/// Where the socket is. `/tmp` is in the initramfs and writable, as `vport`
/// found.
const SOCKET_PATH: &[u8] = b"/tmp/adbd-usb\0";
/// The same without its NUL, for `bind`.
const SOCKET_NAME: &[u8] = b"/tmp/adbd-usb";

/// The largest payload adbd sends over USB (`adbd --usb` offers no more in
/// its `CNXN`): one endpoint ring.
pub(crate) const MAX_PAYLOAD: usize = 4096;

/// A message's header.
const HEADER_BYTES: usize = 24;

/// Bytes kept each way: a whole message and room behind it.
const BUFFER: usize = 2 * (HEADER_BYTES + MAX_PAYLOAD);

/// The bridge: the listening socket, adbd if connected, and what waits each
/// way.
pub(crate) struct Bridge {
    listener: Option<usize>,
    client: Option<usize>,
    /// From adbd, not yet handed to the controller.
    inbound: [u8; BUFFER],
    inbound_len: usize,
    /// After a header went out, the payload that follows it.
    payload_next: Option<usize>,
    /// From the host, not yet written to adbd.
    outbound: [u8; BUFFER],
    outbound_len: usize,
}

impl Bridge {
    /// Listen for adbd. A socket that cannot be made leaves adb off and the
    /// serial port as it was.
    pub(crate) fn new() -> Bridge {
        let listener = listen();
        match listener {
            Some(_) => ferrix_rt::info!("usbdev: adb's interface waits for adbd on /tmp/adbd-usb"),
            None => ferrix_rt::warn!("usbdev: no socket for adbd; adb is off"),
        }
        Bridge {
            listener,
            client: None,
            inbound: [0; BUFFER],
            inbound_len: 0,
            payload_next: None,
            outbound: [0; BUFFER],
            outbound_len: 0,
        }
    }

    /// Whether adbd is connected, so the loop should turn quickly.
    pub(crate) fn connected(&self) -> bool {
        self.client.is_some()
    }

    /// What the host sent on adb's OUT endpoint, for adbd. Kept while adbd
    /// is not yet connected: the host sends its `CNXN` as soon as the
    /// device is configured, which can be before adbd has found the socket,
    /// and it does not send it again.
    pub(crate) fn take_from_host(&mut self, bytes: &[u8]) {
        let room = self
            .outbound
            .get_mut(self.outbound_len..)
            .unwrap_or_default();
        let count = bytes.len().min(room.len());
        if let (Some(into), Some(from)) = (room.get_mut(..count), bytes.get(..count)) {
            into.copy_from_slice(from);
        }
        self.outbound_len += count;
        if count < bytes.len() {
            ferrix_rt::warn!("usbdev: adbd is not reading what the host sends; starting over");
            self.drop_client();
        }
    }

    /// Start over, as after a USB reset.
    pub(crate) fn reset(&mut self) {
        self.drop_client();
    }

    /// Move bytes each way as far as they go now.
    pub(crate) fn pump(&mut self, usb: &mut Usb) -> Result<(), Step> {
        self.accept();
        self.write_adbd();
        self.read_adbd();
        self.send_to_host(usb)
    }

    fn accept(&mut self) {
        if self.client.is_some() {
            return;
        }
        let Some(listener) = self.listener else {
            return;
        };
        if let Ok(fd) = linux::accept4(listener, linux::SOCK_NONBLOCK) {
            ferrix_rt::info!("usbdev: adbd connected");
            self.client = Some(fd);
        }
    }

    fn drop_client(&mut self) {
        if let Some(fd) = self.client.take() {
            let _closed = linux::close(fd);
            ferrix_rt::info!("usbdev: adbd's connection closed");
        }
        self.inbound_len = 0;
        self.outbound_len = 0;
        self.payload_next = None;
    }

    /// The host's bytes to adbd.
    fn write_adbd(&mut self) {
        let Some(fd) = self.client else { return };
        if self.outbound_len == 0 {
            return;
        }
        match linux::write(
            fd,
            self.outbound.get(..self.outbound_len).unwrap_or_default(),
        ) {
            Ok(written) => {
                self.outbound.copy_within(written..self.outbound_len, 0);
                self.outbound_len -= written;
            }
            Err(errno) if errno == Errno::EAGAIN => {}
            Err(_) => self.drop_client(),
        }
    }

    /// adbd's bytes, into the inbound buffer.
    fn read_adbd(&mut self) {
        let Some(fd) = self.client else { return };
        let room = self.inbound.get_mut(self.inbound_len..).unwrap_or_default();
        if room.is_empty() {
            return;
        }
        match linux::read(fd, room) {
            Ok(0) => self.drop_client(),
            Ok(read) => self.inbound_len += read,
            Err(errno) if errno == Errno::EAGAIN => {}
            Err(_) => self.drop_client(),
        }
    }

    /// The next part of adbd's next message to the host, if the last has
    /// gone and this one is whole.
    fn send_to_host(&mut self, usb: &mut Usb) -> Result<(), Step> {
        if self.client.is_none() || usb.pending(ADB_IN) > 0 {
            return Ok(());
        }
        let (part, next) = match self.payload_next {
            Some(length) => (length, None),
            None => {
                let Some(head) = self.inbound.get(..HEADER_BYTES) else {
                    return Ok(());
                };
                if self.inbound_len < HEADER_BYTES {
                    return Ok(());
                }
                match payload_length(head) {
                    Some(0) => (HEADER_BYTES, None),
                    Some(length) => (HEADER_BYTES, Some(length)),
                    None => {
                        ferrix_rt::warn!("usbdev: adbd sent a bad header; dropping it");
                        self.drop_client();
                        return Ok(());
                    }
                }
            }
        };
        if self.inbound_len < part {
            return Ok(());
        }
        let taken = usb
            .write(ADB_IN, self.inbound.get(..part).unwrap_or_default())
            .map_err(|_| Step::Faulted)?;
        if taken == 0 {
            // Not configured yet: the host is not there to read it.
            return Ok(());
        }
        if taken != part {
            ferrix_rt::warn!("usbdev: adb's endpoint took part of a part; dropping adbd");
            self.drop_client();
            return Ok(());
        }
        self.inbound.copy_within(part..self.inbound_len, 0);
        self.inbound_len -= part;
        self.payload_next = next;
        Ok(())
    }
}

/// A header's payload length, if it is a header: the magic word is the
/// command's complement, and the length is at most [`MAX_PAYLOAD`]. The two
/// words `src/lib/proto/adb`'s `Header::decode` checks, read here by hand so
/// the driver needs no allocator.
fn payload_length(head: &[u8]) -> Option<usize> {
    let word = |at: usize| {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(head.get(at..at + 4)?);
        Some(u32::from_le_bytes(bytes))
    };
    let command = word(0)?;
    let length = usize::try_from(word(12)?).ok()?;
    (word(20)? == !command && length <= MAX_PAYLOAD).then_some(length)
}

/// Bind the socket and listen, non-blocking.
fn listen() -> Option<usize> {
    // A path a previous boot left would make `bind` fail.
    let _removed = linux::unlink(SOCKET_PATH);
    let fd = linux::socket(linux::AF_UNIX, linux::SOCK_STREAM | linux::SOCK_NONBLOCK, 0).ok()?;
    let (address, len) = linux::sockaddr_un(SOCKET_NAME)?;
    let _bound = linux::bind(fd, &address, len).ok()?;
    let _listening = linux::listen(fd, 1).ok()?;
    Some(fd)
}
