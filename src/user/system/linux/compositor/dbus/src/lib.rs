//! The D-Bus a desktop daemon needs, and no more (`docs/DESKTOP.md` §4.2).
//!
//! `notifyd` owns `org.freedesktop.Notifications` on the session bus that
//! `sessiond --session-bus` starts, so that libnotify and GTK applications
//! reach it. That takes: the SASL `EXTERNAL` handshake with the caller's
//! uid; `Hello` and `RequestName`; reading method calls and answering them;
//! sending signals. Nothing else of D-Bus -- no introspection data, no
//! properties, no match rules, no file descriptors, no big-endian peers
//! (dbus-daemon writes little-endian on every machine Ferrix runs on).
//!
//! [`wire`] and [`message`] are pure and host-tested; [`Connection`] is the
//! socket.

pub mod message;
pub mod wire;

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

pub use message::{Kind, Message};
pub use wire::{Malformed, Reader, Writer};

/// The bus daemon's own name, path and interface.
pub const BUS_NAME: &str = "org.freedesktop.DBus";
/// `/org/freedesktop/DBus`.
pub const BUS_PATH: &str = "/org/freedesktop/DBus";

/// `RequestName`'s `DBUS_NAME_FLAG_DO_NOT_QUEUE`.
pub const DO_NOT_QUEUE: u32 = 4;
/// `RequestName`'s answer: the name is ours.
pub const PRIMARY_OWNER: u32 = 1;

/// The SASL `AUTH EXTERNAL` line for `uid`: the uid's decimal digits, hex
/// encoded (the D-Bus Specification, "Authentication Protocol").
#[must_use]
pub fn auth_line(uid: u32) -> String {
    let digits = uid.to_string();
    let hex: String = digits.bytes().map(|byte| format!("{byte:02x}")).collect();
    format!("AUTH EXTERNAL {hex}\r\n")
}

/// The session bus's socket path from `DBUS_SESSION_BUS_ADDRESS`'s
/// `unix:path=...` (the first such transport), or `None`.
#[must_use]
pub fn session_path(address: &str) -> Option<String> {
    address.split(';').find_map(|transport| {
        let rest = transport.strip_prefix("unix:")?;
        rest.split(',')
            .find_map(|pair| pair.strip_prefix("path="))
            .map(str::to_owned)
    })
}

/// A connection to a bus.
#[derive(Debug)]
pub struct Connection {
    stream: UnixStream,
    serial: u32,
    pending: Vec<u8>,
    /// The unique name the bus gave us.
    pub unique: String,
}

impl Connection {
    /// Connect to the bus at `path`, authenticate as `uid`, say `Hello`.
    ///
    /// # Errors
    ///
    /// What the socket said, or [`io::ErrorKind::InvalidData`] for a bus
    /// that refused us.
    pub fn open(path: &str, uid: u32) -> io::Result<Self> {
        let mut stream = UnixStream::connect(path)?;
        stream.write_all(b"\0")?;
        stream.write_all(auth_line(uid).as_bytes())?;
        let mut answer = Vec::new();
        let mut byte = [0_u8; 1];
        while !answer.ends_with(b"\r\n") {
            if stream.read(&mut byte)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the bus closed",
                ));
            }
            answer.push(byte[0]);
            if answer.len() > 512 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a long SASL line",
                ));
            }
        }
        if !answer.starts_with(b"OK ") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the bus refused us: {}",
                    String::from_utf8_lossy(&answer).trim()
                ),
            ));
        }
        stream.write_all(b"BEGIN\r\n")?;
        let mut connection = Self {
            stream,
            serial: 0,
            pending: Vec::new(),
            unique: String::new(),
        };
        let hello = Message::call(BUS_NAME, BUS_PATH, BUS_NAME, "Hello");
        let reply = connection.call(&hello)?;
        connection.unique = reply
            .reader()
            .string()
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why.to_string()))?
            .to_owned();
        Ok(connection)
    }

    /// Send `message`, giving the serial it went with.
    ///
    /// # Errors
    ///
    /// What the socket said.
    pub fn send(&mut self, message: &Message) -> io::Result<u32> {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.stream.write_all(&message.encode(self.serial))?;
        Ok(self.serial)
    }

    /// Call and wait for the reply, keeping anything else that arrives
    /// meanwhile for [`Connection::receive`].
    ///
    /// # Errors
    ///
    /// What the socket said, or an error reply as
    /// [`io::ErrorKind::Other`].
    pub fn call(&mut self, message: &Message) -> io::Result<Message> {
        let serial = self.send(message)?;
        let mut kept = Vec::new();
        let reply = loop {
            let next = self.read_one()?;
            if next.reply_serial == Some(serial) {
                break next;
            }
            kept.push(next);
        };
        // Put back what was not the reply, in order, before anything read
        // later.
        let mut put_back = Vec::new();
        for message in kept {
            put_back.extend(message.encode(message.serial));
        }
        put_back.extend(std::mem::take(&mut self.pending));
        self.pending = put_back;
        if reply.kind == Some(Kind::Error) {
            let text = reply.reader().string().unwrap_or_default().to_owned();
            return Err(io::Error::other(format!(
                "{}: {text}",
                reply.error_name.unwrap_or_default()
            )));
        }
        Ok(reply)
    }

    /// `RequestName(name, DO_NOT_QUEUE)`: whether the name is ours.
    ///
    /// # Errors
    ///
    /// What the socket or the bus said.
    pub fn request_name(&mut self, name: &str) -> io::Result<bool> {
        let mut body = Writer::new();
        body.string(name);
        body.u32(DO_NOT_QUEUE);
        let call = Message::call(BUS_NAME, BUS_PATH, BUS_NAME, "RequestName").with_body("su", body);
        let reply = self.call(&call)?;
        Ok(reply.reader().u32().unwrap_or(0) == PRIMARY_OWNER)
    }

    /// The next message, waiting for it.
    ///
    /// # Errors
    ///
    /// What the socket said, or [`io::ErrorKind::InvalidData`].
    pub fn receive(&mut self) -> io::Result<Message> {
        self.read_one()
    }

    /// The socket, for a caller that polls it with others.
    #[must_use]
    pub const fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Whether a whole message is already read and waiting.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        Message::length(&self.pending).is_some_and(|length| self.pending.len() >= length)
    }

    fn read_one(&mut self) -> io::Result<Message> {
        loop {
            if let Some(length) = Message::length(&self.pending)
                && self.pending.len() >= length
            {
                let rest = self.pending.split_off(length);
                let bytes = std::mem::replace(&mut self.pending, rest);
                return Message::decode(&bytes)
                    .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why.to_string()));
            }
            let mut chunk = [0_u8; 4096];
            let got = self.stream.read(&mut chunk)?;
            if got == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the bus closed",
                ));
            }
            self.pending.extend(chunk.get(..got).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests;
