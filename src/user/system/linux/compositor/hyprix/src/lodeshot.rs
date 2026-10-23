//! hyprix as a crash-screenshot provider for lode (`docs/LOGGING.md` §19).
//!
//! When `sessiond` started this compositor for a session it left the
//! session's token in `FERRIX_SESSION_TOKEN`; [`adopt`] takes it out of the
//! environment first thing in `main`, so nothing the compositor starts
//! inherits it. With a token, the compositor connects to lode's
//! `/run/lode/capture`, registers for seat0 with it, and answers each
//! CAPTURE with the first screen as it was last shown, in QOI -- or
//! declines while the session is locked, since what a lock hides is not
//! for the log either. Without a token, or with no lode to talk to, none of
//! this happens and nothing waits for it.
//!
//! lode accepts the registration only from the pid `sessiond` named with
//! that token (§19.1). A lode that goes away is tried again every
//! [`RETRY`]; one that refused is not asked again.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferrix_lode_capture::{FORMAT_QOI, Message, SOCKET, qoi};

/// Where `sessiond` leaves the token.
pub const TOKEN_VARIABLE: &str = "FERRIX_SESSION_TOKEN";

/// How often a lode that went away is looked for again.
const RETRY: Duration = Duration::from_secs(5);

/// Take the session's token out of the environment: lode's socket and it,
/// for [`crate::Options::lode`]. Call it first thing in `main`, before any
/// thread or child exists.
#[must_use]
pub fn adopt() -> Option<(PathBuf, [u8; 16])> {
    let text = std::env::var(TOKEN_VARIABLE).ok()?;
    // SAFETY: `main` calls this before it starts any thread, so nothing
    // reads the environment while it changes.
    unsafe { std::env::remove_var(TOKEN_VARIABLE) };
    parse(&text).map(|token| (PathBuf::from(SOCKET), token))
}

/// 32 hex digits as 16 bytes.
fn parse(text: &str) -> Option<[u8; 16]> {
    let text = text.trim();
    if text.len() != 32 {
        return None;
    }
    let mut token = [0_u8; 16];
    for (index, slot) in token.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(token)
}

/// A request from lode: its number, and the record it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// The request's number.
    pub request: u32,
    /// The crash record's sequence number.
    pub seqnum: u64,
}

/// The connection to lode.
#[derive(Debug)]
pub struct Provider {
    /// lode's socket and the session's token, if this compositor has one.
    lode: Option<(PathBuf, [u8; 16])>,
    /// The connection, while there is one.
    stream: Option<UnixStream>,
    /// What lode has sent of a frame so far.
    buf: Vec<u8>,
    /// When to look for lode again.
    next_try: Instant,
    /// Whether lode refused this compositor: then it is not asked again.
    refused: bool,
}

impl Provider {
    /// A provider for lode at `lode`'s socket with its token; none does
    /// nothing.
    #[must_use]
    pub fn new(lode: Option<(PathBuf, [u8; 16])>) -> Self {
        Provider {
            lode,
            stream: None,
            buf: Vec::new(),
            next_try: Instant::now(),
            refused: false,
        }
    }

    /// The connection's descriptor, to wait on.
    #[must_use]
    pub fn raw_fd(&self) -> Option<RawFd> {
        self.stream.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Connect and register, if there is a token, no connection, and it is
    /// time to look; `outputs` names the screens.
    pub fn connect(&mut self, outputs: &str, report: &mut dyn FnMut(&str)) {
        let Some((path, token)) = self.lode.clone() else {
            return;
        };
        if self.stream.is_some() || self.refused || Instant::now() < self.next_try {
            return;
        }
        self.next_try = Instant::now() + RETRY;
        let Ok(mut stream) = UnixStream::connect(&path) else {
            return;
        };
        let register = Message::Register {
            name: "hyprix".to_owned(),
            seat: "seat0".to_owned(),
            outputs: outputs.to_owned(),
            formats: FORMAT_QOI,
            token,
        };
        if stream.write_all(&register.encode()).is_err() || stream.set_nonblocking(true).is_err() {
            return;
        }
        report("hyprix: offered lode crash screenshots of seat0");
        self.buf.clear();
        self.stream = Some(stream);
    }

    /// What lode sent: the captures it asks for.
    pub fn read(&mut self, report: &mut dyn FnMut(&str)) -> Vec<Request> {
        let mut requests = Vec::new();
        let Some(stream) = self.stream.as_mut() else {
            return requests;
        };
        let mut chunk = [0_u8; 4096];
        let mut gone = false;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    gone = true;
                    break;
                }
                Ok(got) => self
                    .buf
                    .extend_from_slice(chunk.get(..got).unwrap_or_default()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    gone = true;
                    break;
                }
            }
        }
        loop {
            match Message::decode(&self.buf) {
                Ok(Some((message, used))) => {
                    let _ = self.buf.drain(..used);
                    match message {
                        Message::Accepted { .. } => {
                            report(
                                "hyprix: lode took this compositor as seat0's screenshot provider",
                            );
                        }
                        Message::Refused { why } => {
                            report(&format!(
                                "hyprix: lode refused this compositor as a screenshot provider: {why}"
                            ));
                            self.refused = true;
                            gone = true;
                        }
                        Message::Capture {
                            request, seqnum, ..
                        } => requests.push(Request { request, seqnum }),
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    gone = true;
                    break;
                }
            }
        }
        if gone {
            self.stream = None;
        }
        requests
    }

    /// Answer `request` with a `width` by `height` picture whose XRGB rows
    /// are `stride` bytes apart in `pixels`, or decline it saying `why`.
    pub fn answer(&mut self, request: Request, picture: Result<(u32, u32, usize, &[u8]), &str>) {
        let message = match picture {
            Ok((width, height, stride, pixels)) => match qoi::encode(width, height, stride, pixels)
            {
                Some(bytes) if bytes.len() < ferrix_lode_capture::FRAME_MAX - 64 => {
                    Message::Image {
                        request: request.request,
                        format: FORMAT_QOI,
                        width,
                        height,
                        bytes,
                    }
                }
                _ => Message::Declined {
                    request: request.request,
                    why: "the screen did not encode".to_owned(),
                },
            },
            Err(why) => Message::Declined {
                request: request.request,
                why: why.to_owned(),
            },
        };
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        // One picture, written whole: lode reads it without blocking, and a
        // lode that stalls costs this write its patience, not the frames.
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let written = stream.write_all(&message.encode());
        let _ = stream.set_nonblocking(true);
        if written.is_err() {
            self.stream = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn a_token_reads_and_a_bad_one_does_not() {
        assert_eq!(
            parse("000102030405060708090a0b0c0d0e0f"),
            Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
        );
        assert_eq!(parse("short"), None);
    }

    #[test]
    fn registers_answers_a_capture_and_declines_when_told() {
        let dir = std::env::temp_dir().join(format!("hyprix-lodeshot-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("capture");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let mut provider = Provider::new(Some((path.clone(), [5; 16])));
        provider.connect("Virtual-1", &mut |_| {});
        let (mut lode, _) = listener.accept().unwrap();
        let mut buf = vec![0_u8; 4096];
        let got = lode.read(&mut buf).unwrap();
        let (register, _) = Message::decode(&buf[..got]).unwrap().unwrap();
        assert!(matches!(register, Message::Register { token, .. } if token == [5; 16]));
        lode.write_all(
            &Message::Accepted {
                formats: FORMAT_QOI,
            }
            .encode(),
        )
        .unwrap();
        lode.write_all(
            &Message::Capture {
                request: 3,
                seqnum: 40,
                what: "x".into(),
            }
            .encode(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let mut said = Vec::new();
        let requests = provider.read(&mut |line| said.push(line.to_owned()));
        assert_eq!(
            requests,
            [Request {
                request: 3,
                seqnum: 40
            }]
        );
        assert!(
            said.iter()
                .any(|line| line.contains("took this compositor"))
        );
        let screen = [10_u8, 20, 30, 0, 40, 50, 60, 0];
        provider.answer(requests[0], Ok((2, 1, 8, &screen)));
        provider.answer(
            Request {
                request: 4,
                seqnum: 41,
            },
            Err("locked"),
        );
        std::thread::sleep(Duration::from_millis(50));
        let mut answers = Vec::new();
        let mut held = Vec::new();
        lode.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        while let Ok(got) = lode.read(&mut buf) {
            if got == 0 {
                break;
            }
            held.extend_from_slice(&buf[..got]);
            while let Ok(Some((message, used))) = Message::decode(&held) {
                let _ = held.drain(..used);
                answers.push(message);
            }
            if answers.len() == 2 {
                break;
            }
        }
        match &answers[0] {
            Message::Image {
                request: 3,
                width: 2,
                height: 1,
                bytes,
                ..
            } => {
                let (_, _, rgba) = qoi::decode(bytes).unwrap();
                assert_eq!(rgba, [30, 20, 10, 255, 60, 50, 40, 255]);
            }
            other => panic!("not the picture: {other:?}"),
        }
        assert_eq!(
            answers[1],
            Message::Declined {
                request: 4,
                why: "locked".into()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
