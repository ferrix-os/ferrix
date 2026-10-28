//! The helper backend against a fake hyprix-gpu: a thread at the other end
//! of the socket pair that answers as the protocol says, records what it
//! was sent, and fails when a test asks it to.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use compositor_gpu_protocol::{self as proto, Message, Op, Plane};
use compositor_render::{Color, Damage, Format, Painter, Rect, Surface};

use super::conn::Socket;
use super::{Canvas, Link};

/// What the fake does with a frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frames {
    /// Answers `Done`.
    Done,
    /// Never answers.
    Silent,
    /// Closes its end, as a helper that crashed.
    Die,
    /// Answers `Failed` with device lost.
    Lost,
}

/// Everything the fake was sent, message by message, ops decoded.
#[derive(Debug)]
enum Seen {
    Message(Message),
    Op(Op),
}

fn fake(socket: OwnedFd, frames: Frames, seen: mpsc::Sender<Seen>) {
    let mut socket = Socket::new(socket);
    let mut outputs: Vec<OwnedFd> = Vec::new();
    loop {
        let deadline = Instant::now() + Duration::from_secs(5);
        let Ok(Some(received)) = socket.receive(deadline) else {
            return;
        };
        let reply = match &received.message {
            Message::Hello { .. } => {
                let _ = socket.send(
                    &Message::Ready {
                        version: proto::VERSION,
                        caps: 0,
                        device: "fake".to_owned(),
                        driver: "test".to_owned(),
                    },
                    &[],
                );
                for fourcc in [proto::ARGB8888, proto::XRGB8888] {
                    let _ = socket.send(
                        &Message::Modifiers {
                            fourcc,
                            modifiers: vec![(0, false)],
                        },
                        &[],
                    );
                }
                None
            }
            Message::Configure {
                canvas,
                width,
                height,
                ..
            } => {
                let Ok(memory) = compositor_shm::Shared::new((width * height * 4) as usize) else {
                    return;
                };
                let Ok(fd) = std::fs::File::open("/dev/null").map(OwnedFd::from) else {
                    return;
                };
                drop(memory);
                outputs.push(fd);
                let borrowed: Vec<_> = outputs.iter().map(std::os::fd::AsFd::as_fd).collect();
                let _ = socket.send(
                    &Message::Outputs {
                        canvas: *canvas,
                        width: *width,
                        height: *height,
                        fourcc: proto::XRGB8888,
                        modifier: 0,
                        planes: vec![Plane {
                            stride: width * 4,
                            offset: 0,
                        }],
                    },
                    &borrowed[borrowed.len() - 1..],
                );
                None
            }
            Message::Ops(body) => {
                for op in Op::decode_all(body).unwrap_or_default() {
                    let _ = seen.send(Seen::Op(op));
                }
                None
            }
            Message::FrameEnd { frame, .. } => match frames {
                Frames::Done => Some(Message::Done {
                    canvas: 1,
                    frame: *frame,
                    output: 0,
                    flags: 0,
                }),
                Frames::Silent => None,
                Frames::Die => return,
                Frames::Lost => Some(Message::Failed {
                    canvas: 1,
                    frame: *frame,
                    code: proto::failure::DEVICE_LOST,
                    why: "lost".to_owned(),
                }),
            },
            _ => None,
        };
        if !matches!(received.message, Message::Ops(_)) {
            let _ = seen.send(Seen::Message(received.message));
        }
        if let Some(reply) = reply {
            let _ = socket.send(&reply, &[]);
        }
    }
}

/// A canvas on a fake helper that treats frames as `frames`, and what the
/// fake sees.
fn rig(frames: Frames) -> (Canvas, Rc<RefCell<Link>>, mpsc::Receiver<Seen>) {
    let (ours, theirs) = Socket::pair().unwrap_or_else(|error| panic!("socketpair: {error}"));
    let (tell, seen) = mpsc::channel();
    let _ = std::thread::spawn(move || fake(theirs, frames, tell));
    let link = Rc::new(RefCell::new(
        Link::over(ours).unwrap_or_else(|error| panic!("handshake: {error}")),
    ));
    let canvas = Canvas::new(Rc::clone(&link), 1, 64, 48)
        .unwrap_or_else(|error| panic!("configure: {error}"));
    (canvas, link, seen)
}

/// A frame: a clear, a fill and a 16x16 shared-memory surface.
fn scene(canvas: &mut Canvas, pixels: &[u8]) {
    let whole = Damage::from(Rect::new(0, 0, 64, 48));
    canvas.clear(Color(0xff00_0000), &whole);
    canvas.fill(Rect::new(4, 4, 10, 10), Color(0x80ff_0000), &whole);
    let surface = Surface::new(pixels, 16, 16, 64, Format::Xrgb8888)
        .unwrap_or_else(|error| panic!("surface: {error:?}"))
        .named(7);
    canvas.composite(&surface, Rect::new(20, 20, 16, 16), &whole);
}

#[test]
fn a_frame_is_its_painter_calls_and_the_pixels_they_read() {
    let (mut canvas, link, seen) = rig(Frames::Done);
    assert_eq!(link.borrow().device, "fake");
    assert!(link.borrow().samples(proto::XRGB8888, 0));
    let pixels = vec![0x55; 16 * 64];
    scene(&mut canvas, &pixels);
    assert!(canvas.finish().is_ok());
    let first: Vec<Seen> = seen.try_iter().collect();
    let ops: Vec<&Op> = first
        .iter()
        .filter_map(|seen| match seen {
            Seen::Op(op) => Some(op),
            Seen::Message(_) => None,
        })
        .collect();
    assert!(matches!(ops.first(), Some(Op::Damage(rects)) if rects.len() == 1));
    assert!(matches!(ops.get(1), Some(Op::Clear(0xff00_0000))));
    assert!(matches!(ops.get(2), Some(Op::Fill(_, 0x80ff_0000))));
    assert!(
        matches!(ops.get(3), Some(Op::Upload { texture: 7, rect, .. }) if rect.width == 16 && rect.height == 16)
    );
    assert!(matches!(
        ops.get(4),
        Some(Op::Composite { texture: 7, rect, .. }) if rect.x == 20 && rect.y == 20
    ));
    assert!(first.iter().any(|seen| matches!(
        seen,
        Seen::Message(Message::CreateTexture { texture: 7, width: 16, .. })
    )));
    assert!(first.iter().any(|seen| matches!(seen, Seen::Message(Message::Arena { .. }))));
    // A second frame: the texture is kept, only the damage under it moves.
    let part = Damage::from(Rect::new(20, 20, 4, 4));
    let surface = Surface::new(&pixels, 16, 16, 64, Format::Xrgb8888)
        .unwrap_or_else(|error| panic!("surface: {error:?}"))
        .named(7);
    canvas.composite(&surface, Rect::new(20, 20, 16, 16), &part);
    assert!(canvas.finish().is_ok());
    let again: Vec<Seen> = seen.try_iter().collect();
    assert!(!again.iter().any(|seen| matches!(seen, Seen::Message(Message::CreateTexture { .. }))));
    assert!(again.iter().any(|seen| matches!(
        seen,
        Seen::Op(Op::Upload { rect, .. }) if rect.width == 4 && rect.height == 4
    )));
    // The card is told it may have the output back before it is drawn again.
    assert!(again.iter().any(|seen| matches!(seen, Seen::Message(Message::Release { .. }))));
}

#[test]
fn a_helper_that_dies_fails_the_frame_and_every_one_after() {
    let (mut canvas, link, _seen) = rig(Frames::Die);
    scene(&mut canvas, &[0; 16 * 64]);
    assert!(canvas.finish().is_err());
    assert!(link.borrow().dead().is_some());
    scene(&mut canvas, &[0; 16 * 64]);
    let started = Instant::now();
    assert!(canvas.finish().is_err());
    assert!(started.elapsed() < Duration::from_millis(50));
}

#[test]
fn a_helper_that_never_answers_fails_the_frame_in_time() {
    let (mut canvas, _link, _seen) = rig(Frames::Silent);
    scene(&mut canvas, &[0; 16 * 64]);
    let started = Instant::now();
    let error = canvas.finish().err();
    assert_eq!(error.map(|error| error.kind()), Some(std::io::ErrorKind::TimedOut));
    assert!(started.elapsed() >= super::canvas::FRAME_DEADLINE);
    assert!(started.elapsed() < super::canvas::FRAME_DEADLINE * 4);
}

#[test]
fn a_lost_device_leaves_the_helper_dead() {
    let (mut canvas, link, _seen) = rig(Frames::Lost);
    scene(&mut canvas, &[0; 16 * 64]);
    assert!(canvas.finish().is_err());
    assert!(link.borrow().dead().is_some());
}

#[test]
fn an_empty_frame_sends_nothing() {
    let (mut canvas, _link, seen) = rig(Frames::Silent);
    let _: Vec<Seen> = seen.try_iter().collect();
    assert!(canvas.finish().is_ok());
    std::thread::sleep(Duration::from_millis(50));
    assert!(seen.try_iter().next().is_none());
}

#[test]
fn hyprix_gpu_reads_as_a_mode() {
    use super::Mode;
    assert_eq!(Mode::parse("off"), Mode::Off);
    assert_eq!(Mode::parse(" auto "), Mode::Auto);
    assert_eq!(Mode::parse("on"), Mode::On);
    assert_eq!(Mode::parse("bogus"), Mode::Off);
}
