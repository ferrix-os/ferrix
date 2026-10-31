//! `pipe`, `pipe2`, `sendfile`, `splice` and `copy_file_range`.
//!
//! The pipe itself is `crate::fs::pipe`. What is here is making one into two
//! descriptors, and the three calls that move bytes from one descriptor to
//! another without the program ever holding them.
//!
//! # Two descriptors, or none
//!
//! `pipe2` writes both numbers into the program's memory once both are
//! installed. If that write faults the program cannot learn the numbers, so
//! both are closed again before `EFAULT` is returned, and a failed call leaves
//! the table as it found it.
//!
//! # `sendfile` through a kernel buffer
//!
//! Linux splices pages from one file to the other. Nothing here has a page
//! cache to splice from, so the bytes go through a buffer of at most
//! [`SENDFILE_CHUNK`]: `read` and `write` through a bounce buffer, with no copy
//! to the program in between. A short write to the output puts the input's
//! position back over what the output did not take, where the input has a
//! position. Into a pipe it is one `splice` into a pipe, as Linux's is: the
//! input is read for no more than the pipe has room for, so a socket or a
//! terminal, which cannot be put back, gives only what the pipe will take.
//!
//! An input with no position has nowhere to put them back, and Linux does not
//! take one: its `splice_direct_to_actor` requires an input that can seek, "as
//! we don't want to randomly drop data for eg socket -> socket splicing". So
//! into anything but a pipe, `sendfile` from a pipe, a socket, a terminal or an
//! anonymous object is `EINVAL`, and into a pipe, where Linux splices without
//! that rule, from a pipe or an anonymous object, which have no `splice_read`.
//! Nor do `/dev/null`, the DRM and event devices, or most of a process's
//! procfs files, and those are `EINVAL` into anything (`splices_out`).
//!
//! ARMv7-A numbers only `sendfile64`, whose offset is a 64-bit `loff_t` like
//! the 64-bit architectures' `off_t`, so every offset read here is eight bytes.
//!
//! # `splice` and `copy_file_range` the same way
//!
//! Both go through a kernel buffer as `sendfile` does. Between two pipes
//! `splice` moves bytes under both pipes' locks, so none is ever out of both;
//! into a pipe it reads no more than the pipe has room for, so a call never
//! waits on itself; out of a pipe it reads what is there once, and puts back
//! at the pipe's front what the output refuses, as Linux leaves it in the
//! pipe. GNU grep is why
//! `splice` is here: writing to `/dev/null` from a pipe, it drains the rest of
//! its input with `splice` and falls back to `read` only on `EINVAL`, so an
//! `ENOSYS` was an error, and curl's `configure` concluded there was no grep.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{O_CLOEXEC, O_NONBLOCK, SPLICE_F_ALL, SPLICE_F_NONBLOCK};
use ferrix_vfs::{FileType, OpenFile, Whence};

use crate::fs;
use crate::syscall::fd;
use crate::syscall::process::Process;
use crate::syscall::uaccess;

/// The most one `sendfile` holds in the kernel at once: sixteen pages.
const SENDFILE_CHUNK: usize = 16 * 4096;

/// Linux's `MAX_RW_COUNT`, which `sendfile` clamps to as `read` and `write`
/// do, so that the count always fits a 32-bit return register.
const MAX_RW_COUNT: u64 = 0x7FFF_F000;

/// `pipe2`, and `pipe` as `pipe2` with no flags.
///
/// `O_CLOEXEC` and `O_NONBLOCK` are taken, both decoded through the
/// architecture's `open` table, where all three architectures agree on them:
/// neither `arch/arm/include/uapi/asm/fcntl.h` nor its arm64 counterpart
/// overrides the generic header's values, and x86-64 has no header of its own.
/// So is `O_DIRECT`, whose bit Arm does move, for a packet pipe that keeps
/// each write apart (`ferrix_vfs::pipe`): the Steam client's controller code
/// asserts without one. Linux's `O_NOTIFICATION_PIPE` is `EINVAL` here, with
/// every other bit.
pub(crate) fn sys_pipe2(process: &Process, fds: u64, raw: u32) -> Result<usize, Errno> {
    // `O_DIRECT` is this architecture's bit, which is not the same on Arm.
    let direct = crate::arch::OPEN_FLAGS.direct;
    if raw & !(O_CLOEXEC | O_NONBLOCK | direct) != 0 {
        return Err(Errno::EINVAL);
    }
    let (flags, cloexec) = fd::decode_open_flags(raw);
    let owner = crate::syscall::path::creator_ids(process);
    let packets = raw & direct != 0;
    let (reader, writer) = fs::pipe::new_pipe_of(flags.nonblock, packets, owner)?;
    let (read_fd, write_fd) = install(process, reader, writer, cloexec)?;

    let pair: Vec<u8> = read_fd
        .to_le_bytes()
        .into_iter()
        .chain(write_fd.to_le_bytes())
        .collect();
    if uaccess::copy_to_user(process.space(), fds, &pair).is_err() {
        // Taken out under the lock, dropped after it: see `fd`.
        let taken = {
            let mut files = process.files().lock();
            (files.remove(read_fd), files.remove(write_fd))
        };
        drop(taken);
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

/// Put both ends in the table in one hold of its lock, or neither.
///
/// An end a full table refuses is dropped inside the lock, which for a pipe
/// end is bounded: it closes a pipe and wakes its queues, and holds no dentry
/// in any tree.
fn install(
    process: &Process,
    reader: Arc<OpenFile>,
    writer: Arc<OpenFile>,
    cloexec: bool,
) -> Result<(i32, i32), Errno> {
    let mut files = process.files().lock();
    let read_fd = files.insert(reader, cloexec)?;
    match files.insert(writer, cloexec) {
        Ok(write_fd) => Ok((read_fd, write_fd)),
        Err(errno) => {
            let displaced = files.remove(read_fd);
            drop(files);
            drop(displaced);
            Err(errno)
        }
    }
}

/// `sendfile` and `sendfile64`: copy up to `count` bytes from `in_fd` to
/// `out_fd`.
///
/// With an offset pointer the input is read from that offset, the offset is
/// written back past what was sent, and the input's own position is left
/// alone. Without one, the input's position moves.
pub(crate) fn sys_sendfile(
    process: &Process,
    out_fd: i32,
    in_fd: i32,
    offset_at: u64,
    count: u64,
) -> Result<usize, Errno> {
    let start = if offset_at == 0 {
        None
    } else {
        let mut bytes = [0_u8; 8];
        uaccess::copy_from_user(process.space(), offset_at, &mut bytes)
            .map_err(|_| Errno::EFAULT)?;
        Some(i64::from_le_bytes(bytes))
    };
    let (sent, end) = transfer(process, out_fd, in_fd, start, count)?;
    if let Some(end) = end {
        uaccess::copy_to_user(process.space(), offset_at, &end.to_le_bytes())
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(sent)
}

/// Check both descriptors in Linux's order, then copy. Answers what was sent,
/// and where the offset ended for a call that gave one.
///
/// `EBADF` for an input not open for reading or an output not open for
/// writing, as `sendfile(2)` documents; `ESPIPE` for an offset on an input
/// with no position; `EINVAL` for a negative offset, for a directory as input,
/// an output under `O_APPEND`, which Linux's splice refuses, and an input
/// Linux does not send from ([`refuses_input`]).
fn transfer(
    process: &Process,
    out_fd: i32,
    in_fd: i32,
    start: Option<i64>,
    count: u64,
) -> Result<(usize, Option<u64>), Errno> {
    let input = fd::file(process, in_fd)?;
    if !input.readable() {
        return Err(Errno::EBADF);
    }
    if start.is_some() && !input.takes_offsets() {
        return Err(Errno::ESPIPE);
    }
    let mut position = start
        .map(|at| u64::try_from(at).map_err(|_| Errno::EINVAL))
        .transpose()?;
    let output = fd::file(process, out_fd)?;
    if !output.writable() {
        return Err(Errno::EBADF);
    }
    if output.status().append
        || input.kind() == FileType::Directory
        || refuses_input(&input, &output, count)
    {
        return Err(Errno::EINVAL);
    }
    let count = usize::try_from(count.min(MAX_RW_COUNT)).unwrap_or(SENDFILE_CHUNK);
    let sent = if fs::pipe::is_pipe(&output) {
        // Linux's `splice_file_to_pipe`: wait for room unless the pipe is
        // non-blocking, then read no more than it has room for.
        let nonblock = output.status().nonblock;
        into_a_pipe(&input, &output, position.as_mut(), count, nonblock)?
    } else {
        copy(&input, &output, position.as_mut(), count)?
    };
    Ok((sent, position))
}

/// Whether Linux's `sendfile` refuses to read `input` for `output`.
///
/// Into anything but a pipe, Linux's `do_splice_direct` takes only an input
/// that can seek, and so refuses every stream but the memory devices, whose
/// position never moves. Into a pipe it splices from anything with a
/// `splice_read`, which a socket and a terminal have and a pipe and an
/// anonymous object -- an eventfd, a timerfd, a signalfd, an epoll, a sync
/// file -- do not; there a count of zero is answered before the input is
/// looked at. Measured on a 7.0 host, a pipe holding five bytes is `EINVAL`
/// into a file, a socket and a pipe, and still holds five; a Unix socket of
/// each type and a pseudoterminal's either end are `EINVAL` into a file or a
/// socket and send into a pipe; an eventfd, a timerfd, a signalfd, an epoll
/// and an inotify are `EINVAL` into all three, and 0 into a pipe for a count
/// of zero; `/dev/zero`, `/dev/urandom` and `/dev/full` send into all three.
/// Ferrix used to copy from all of them, and to lose what a short write left
/// of a pipe's bytes.
///
/// A file with no `splice_read` of its own ([`splices_out`]) is refused the
/// same way into anything, for a count above zero: `/dev/null`, whose read is
/// end of file, and the procfs files Linux reads through `seq_read`, which
/// used to be sent from here as the files they read as.
fn refuses_input(input: &OpenFile, output: &OpenFile, count: u64) -> bool {
    if !fs::pipe::is_pipe(output) && !input.takes_offsets() {
        return true;
    }
    count > 0 && !splices_out(input)
}

/// Whether Linux's file for `input` has a `splice_read`, which `sendfile`
/// and `splice` read through: a pipe and a stream on the anonymous
/// filesystem have none, and an object says for itself
/// ([`ferrix_vfs::Inode::splices_out`]). Measured on a 7.0 host, `sendfile`
/// of five bytes from each of these is `EINVAL` into a file, a pipe and a
/// Unix socket, and a count of zero is 0: `/dev/null`, a DRM render node and
/// card, an event device, `/proc/net/{arp,dev,route,tcp,udp,tcp6,udp6}`,
/// and `/proc/self/{status,stat,comm,cmdline,maps,cgroup,oom_score_adj}`
/// and a thread's `status`, `stat` and `comm`. `/proc/self/mounts`, every
/// other top-level `/proc` file, every value under `/proc/sys`, and sysfs and
/// cgroupfs files send five, as `/dev/zero`, `full`, `random` and `urandom`
/// do. `splice` into a pipe answers the same.
fn splices_out(input: &OpenFile) -> bool {
    !fs::pipe::is_pipe(input)
        && !(input.is_stream() && fs::anon::holds(input))
        && input.io().splices_out()
}

/// Move up to `count` bytes, a chunk at a time, stopping at end of file, at a
/// short write, or after one chunk from a stream.
fn copy(
    input: &OpenFile,
    output: &OpenFile,
    mut position: Option<&mut u64>,
    count: usize,
) -> Result<usize, Errno> {
    let mut buffer =
        crate::fallible::try_filled(0_u8, count.min(SENDFILE_CHUNK)).map_err(|_| Errno::ENOMEM)?;
    let mut sent = 0;
    while sent < count {
        let want = (count - sent).min(buffer.len());
        let slot = buffer.get_mut(..want).ok_or(Errno::EIO)?;
        let read = match position.as_deref() {
            Some(&at) => input.read_at(at, slot),
            None => input.read(slot),
        };
        let got = match read {
            Ok(0) => break,
            Ok(got) => got,
            Err(errno) => return partial(sent, errno),
        };
        let chunk = buffer.get(..got).ok_or(Errno::EIO)?;
        let wrote = match output.write(chunk) {
            Ok(wrote) => wrote,
            Err(errno) => {
                give_back(input, position.is_some(), got);
                return partial(sent, errno);
            }
        };
        sent += wrote;
        if let Some(at) = position.as_deref_mut() {
            *at = at.saturating_add(wrote as u64);
        }
        if wrote < got {
            give_back(input, position.is_some(), got - wrote);
            break;
        }
        if input.is_stream() {
            break;
        }
    }
    Ok(sent)
}

/// Put the input's position back over `unread` bytes it read and nobody
/// wrote. Only a call without an offset moved the position, and only an input
/// with a position can be put back.
fn give_back(input: &OpenFile, positioned: bool, unread: usize) {
    if positioned || input.is_stream() {
        return;
    }
    if let Ok(back) = i64::try_from(unread) {
        let _ = input.seek(-back, Whence::Current);
    }
}

/// A count so far, or `errno` if nothing was done.
fn partial(done: usize, errno: Errno) -> Result<usize, Errno> {
    if done > 0 { Ok(done) } else { Err(errno) }
}

/// `splice`: move up to `len` bytes between two descriptors, at least one of
/// them a pipe.
///
/// Checked in Linux's order: an empty move is done before anything is looked
/// at, then the flags, the two descriptors, an offset given for a pipe
/// (`ESPIPE`), the offsets' memory (`EFAULT`), and each end's access mode
/// (`EBADF`). Two ends of one pipe, two descriptors neither of which is a
/// pipe, an offset on a stream, a negative offset, an output under
/// `O_APPEND`, a directory as input, and an input Linux cannot splice from
/// ([`splices_out`]) are `EINVAL`.
///
/// `SPLICE_F_NONBLOCK` makes the pipe side not wait, and so does the pipe's
/// own `O_NONBLOCK`; between two pipes, either one's. The other descriptor's
/// `O_NONBLOCK` governs only its own side: a socket read without data, or a
/// write to a full socket. `SPLICE_F_NONBLOCK` and the pipe's flag also stop
/// a socket read from waiting, but never a socket write. Measured on a Linux
/// 7.0 host over the whole matrix -- socket, file and pipe into a pipe, a
/// pipe into each, each flag and each end's `O_NONBLOCK` -- and matched
/// cell for cell. The other hints are taken and ignored, as `SPLICE_F_MOVE`
/// has been on Linux since 2.6.21.
pub(crate) fn sys_splice(
    process: &Process,
    in_fd: i32,
    in_offset_at: u64,
    out_fd: i32,
    out_offset_at: u64,
    len: u64,
    flags: u32,
) -> Result<usize, Errno> {
    if len == 0 {
        return Ok(0);
    }
    if flags & !SPLICE_F_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    let input = fd::file(process, in_fd)?;
    let output = fd::file(process, out_fd)?;
    let from_pipe = fs::pipe::is_pipe(&input);
    let to_pipe = fs::pipe::is_pipe(&output);
    if (from_pipe && in_offset_at != 0) || (to_pipe && out_offset_at != 0) {
        return Err(Errno::ESPIPE);
    }
    let in_start = read_offset(process, in_offset_at)?;
    let out_start = read_offset(process, out_offset_at)?;
    if !input.readable() || !output.writable() {
        return Err(Errno::EBADF);
    }
    let len = clamp(len);
    let nonblock = flags & SPLICE_F_NONBLOCK != 0;
    match (from_pipe, to_pipe) {
        (true, true) => {
            if fs::pipe::same_pipe(&input, &output) {
                return Err(Errno::EINVAL);
            }
            let nonblock = nonblock || input.status().nonblock || output.status().nonblock;
            fs::pipe::splice_pipes(&input, &output, len, nonblock)
        }
        (true, false) => {
            let mut at = position(&output, out_start)?;
            if output.status().append {
                return Err(Errno::EINVAL);
            }
            let nonblock = nonblock || input.status().nonblock;
            let moved = out_of_a_pipe(&input, &output, at.as_mut(), len, nonblock)?;
            write_offset(process, out_offset_at, at)?;
            Ok(moved)
        }
        (false, true) => {
            let mut at = position(&input, in_start)?;
            if input.kind() == FileType::Directory || !splices_out(&input) {
                return Err(Errno::EINVAL);
            }
            let nonblock = nonblock || output.status().nonblock;
            let moved = into_a_pipe(&input, &output, at.as_mut(), len, nonblock)?;
            write_offset(process, in_offset_at, at)?;
            Ok(moved)
        }
        (false, false) => Err(Errno::EINVAL),
    }
}

/// `copy_file_range`: copy up to `len` bytes of one regular file into
/// another, from and to the offsets given or else each file's position,
/// moving whichever was used past what was copied.
///
/// Checked in Linux's order: the two descriptors, the offsets' memory, the
/// flags, which must be zero, then a directory (`EISDIR`) or anything else
/// that is not a regular file (`EINVAL`), then the access modes and an
/// output under `O_APPEND` (`EBADF`), a negative offset, and ranges that
/// overlap in one file (`EINVAL`). The copy stops at the input's end, so
/// from there it answers 0.
///
/// Linux refuses a copy between two filesystems with `EXDEV` unless the
/// filesystem copies for itself, and programs fall back to reading and
/// writing. Every copy here reads and writes, so none is refused for that.
pub(crate) fn sys_copy_file_range(
    process: &Process,
    in_fd: i32,
    in_offset_at: u64,
    out_fd: i32,
    out_offset_at: u64,
    len: u64,
    flags: u32,
) -> Result<usize, Errno> {
    let input = fd::file(process, in_fd)?;
    let output = fd::file(process, out_fd)?;
    let in_start = read_offset(process, in_offset_at)?;
    let out_start = read_offset(process, out_offset_at)?;
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    if input.kind() == FileType::Directory || output.kind() == FileType::Directory {
        return Err(Errno::EISDIR);
    }
    if input.kind() != FileType::Regular || output.kind() != FileType::Regular {
        return Err(Errno::EINVAL);
    }
    if !input.readable() || !output.writable() || output.status().append {
        return Err(Errno::EBADF);
    }
    let from = match in_start {
        Some(at) => u64::try_from(at).map_err(|_| Errno::EINVAL)?,
        None => input.offset(),
    };
    let to = match out_start {
        Some(at) => u64::try_from(at).map_err(|_| Errno::EINVAL)?,
        None => output.offset(),
    };
    let len = clamp(len);
    let wide = len as u64;
    let overlap = to < from.saturating_add(wide) && from < to.saturating_add(wide);
    if overlap && same_file(&input, &output) {
        return Err(Errno::EINVAL);
    }
    let size = input.inode().metadata().size;
    let count = usize::try_from(size.saturating_sub(from))
        .unwrap_or(usize::MAX)
        .min(len);
    let (copied, stopped) = copy_between(&input, &output, from, to, count);
    if copied > 0 {
        let past = |at: u64| at.saturating_add(copied as u64);
        match in_start {
            Some(_) => write_offset(process, in_offset_at, Some(past(from)))?,
            None => set_position(&input, past(from)),
        }
        match out_start {
            Some(_) => write_offset(process, out_offset_at, Some(past(to)))?,
            None => set_position(&output, past(to)),
        }
    }
    match stopped {
        Some(errno) => partial(copied, errno),
        None => Ok(copied),
    }
}

/// A count as `read` and `write` take one: at most [`MAX_RW_COUNT`].
fn clamp(len: u64) -> usize {
    usize::try_from(len.min(MAX_RW_COUNT)).unwrap_or(SENDFILE_CHUNK)
}

/// The offset a `loff_t *` argument points at, or `None` for a null one.
fn read_offset(process: &Process, at: u64) -> Result<Option<i64>, Errno> {
    if at == 0 {
        return Ok(None);
    }
    let mut bytes = [0_u8; 8];
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    Ok(Some(i64::from_le_bytes(bytes)))
}

/// Write `end` back through a `loff_t *` argument, if the call was given one.
fn write_offset(process: &Process, at: u64, end: Option<u64>) -> Result<(), Errno> {
    match end {
        Some(end) if at != 0 => uaccess::copy_to_user(process.space(), at, &end.to_le_bytes())
            .map_err(|_| Errno::EFAULT),
        _ => Ok(()),
    }
}

/// Where `splice` reads or writes the file that is not a pipe: at the offset
/// given, which a stream cannot take -- but for `/dev/zero` and the other
/// memory devices, whose position never moves -- and which must not be
/// negative, or else at the file's own position.
fn position(file: &OpenFile, start: Option<i64>) -> Result<Option<u64>, Errno> {
    let Some(at) = start else {
        return Ok(None);
    };
    if !file.takes_offsets() {
        return Err(Errno::EINVAL);
    }
    u64::try_from(at).map(Some).map_err(|_| Errno::EINVAL)
}

/// One `splice` out of a pipe: what the pipe holds now, up to `len` and a
/// chunk, read once and written to `output` at `at` or its position.
///
/// What the output does not take goes back to the front of the pipe
/// (`Inode::unread_stream`), as Linux leaves it there -- measured on a 7.0
/// host: a splice of six bytes into a full non-blocking socket is `EAGAIN`
/// and the pipe still holds six, and of 200 bytes into a file that
/// `RLIMIT_FSIZE` holds to 100 is 100, the pipe still holding the other
/// 100, and the next splice `EFBIG` with them still there. This used to lose
/// them. The pipe cannot keep the order if a second reader of it takes
/// bytes in between, which Linux's pipe lock would prevent.
fn out_of_a_pipe(
    input: &OpenFile,
    output: &OpenFile,
    at: Option<&mut u64>,
    len: usize,
    nonblock: bool,
) -> Result<usize, Errno> {
    let mut buffer =
        crate::fallible::try_filled(0_u8, len.min(SENDFILE_CHUNK)).map_err(|_| Errno::ENOMEM)?;
    let got = fs::pipe::read(input, &mut buffer, nonblock)?;
    let chunk = buffer.get(..got).ok_or(Errno::EIO)?;
    let mut put = 0;
    let mut refused = None;
    while put < chunk.len() {
        let rest = chunk.get(put..).unwrap_or_default();
        let wrote = match at.as_deref() {
            Some(&start) => output.write_at(start.saturating_add(put as u64), rest),
            None => output.write(rest),
        };
        match wrote {
            Ok(0) => break,
            Ok(wrote) => put += wrote,
            Err(errno) => {
                refused = Some(errno);
                break;
            }
        }
    }
    input
        .io()
        .unread_stream(chunk.get(put..).unwrap_or_default());
    if let Some(at) = at {
        *at = at.saturating_add(put as u64);
    }
    match refused {
        Some(errno) => partial(put, errno),
        None => Ok(put),
    }
}

/// One `splice` into a pipe, and a `sendfile` into one: no more than the
/// pipe has room for, so the call never waits on itself and a stream is never
/// read for more than can be put down, read from `input` at `at` or its
/// position and queued. A stream is read without waiting under `nonblock`,
/// as Linux's socket reads for a splice under `SPLICE_F_NONBLOCK`.
///
/// What the pipe did not take goes back to the input's position, where it
/// has one. A stream has none -- a socket or a terminal cannot un-read -- so
/// what it gave is put down whatever that waits for ([`keep`]). The room is
/// measured first, so that takes a second writer to the same pipe filling it
/// in between; Linux holds the pipe's lock across the read, which this pipe,
/// a spin lock, cannot be held across.
fn into_a_pipe(
    input: &OpenFile,
    output: &OpenFile,
    at: Option<&mut u64>,
    len: usize,
    nonblock: bool,
) -> Result<usize, Errno> {
    let room = fs::pipe::room(output, nonblock)?;
    let mut buffer = crate::fallible::try_filled(0_u8, len.min(room).min(SENDFILE_CHUNK))
        .map_err(|_| Errno::ENOMEM)?;
    let got = match at.as_deref() {
        Some(&start) => input.read_at(start, &mut buffer)?,
        None if input.is_stream() => input
            .io()
            .read_stream(&mut buffer, nonblock || input.status().nonblock)?,
        None => input.read(&mut buffer)?,
    };
    let chunk = buffer.get(..got).ok_or(Errno::EIO)?;
    if chunk.is_empty() {
        return Ok(0);
    }
    let unreadable = at.is_none() && !input.takes_offsets();
    let first = fs::pipe::write(output, chunk, nonblock);
    let wrote = match first {
        Ok(wrote) => wrote,
        Err(errno) if !unreadable || errno == Errno::EPIPE => {
            give_back(input, at.is_some(), got);
            return Err(errno);
        }
        Err(_) => 0,
    };
    let wrote = if wrote < got && unreadable {
        wrote + keep(output, chunk.get(wrote..).unwrap_or_default())
    } else {
        if wrote < got {
            give_back(input, at.is_some(), got - wrote);
        }
        wrote
    };
    if let (0, Err(errno)) = (wrote, first) {
        return Err(errno);
    }
    if let Some(at) = at {
        *at = at.saturating_add(wrote as u64);
    }
    Ok(wrote)
}

/// Put `rest` in the pipe `output` is, waiting for room: what a stream gave
/// and cannot take back. Answers how much went in, all of it unless the last
/// reader left or the caller was interrupted, where what is left has nowhere
/// to go.
fn keep(output: &OpenFile, rest: &[u8]) -> usize {
    let mut put = 0;
    while let Some(left) = rest.get(put..).filter(|left| !left.is_empty()) {
        match fs::pipe::write(output, left, false) {
            Ok(0) | Err(_) => break,
            Ok(wrote) => put += wrote,
        }
    }
    put
}

/// Whether two open files are one file: the same inode on the same
/// filesystem.
fn same_file(a: &OpenFile, b: &OpenFile) -> bool {
    let device = |file: &OpenFile| file.location().mount.filesystem().device();
    a.inode().metadata().ino == b.inode().metadata().ino && device(a) == device(b)
}

/// Copy `count` bytes from `from` in `input` to `to` in `output`, a chunk at
/// a time. Answers how much was copied, and the error that stopped it early.
fn copy_between(
    input: &OpenFile,
    output: &OpenFile,
    from: u64,
    to: u64,
    count: usize,
) -> (usize, Option<Errno>) {
    let Ok(mut buffer) = crate::fallible::try_filled(0_u8, count.min(SENDFILE_CHUNK)) else {
        return (0, Some(Errno::ENOMEM));
    };
    let mut copied = 0;
    while copied < count {
        let want = (count - copied).min(buffer.len());
        let Some(slot) = buffer.get_mut(..want) else {
            return (copied, Some(Errno::EIO));
        };
        let got = match input.read_at(from.saturating_add(copied as u64), slot) {
            Ok(0) => break,
            Ok(got) => got,
            Err(errno) => return (copied, Some(errno)),
        };
        let mut put = 0;
        while put < got {
            let rest = slot.get(put..got).unwrap_or_default();
            match output.write_at(to.saturating_add((copied + put) as u64), rest) {
                Ok(0) => return (copied + put, None),
                Ok(wrote) => put += wrote,
                Err(errno) => return (copied + put, Some(errno)),
            }
        }
        copied += got;
    }
    (copied, None)
}

/// Move a file's position to `at`, for a copy that was given no offset.
fn set_position(file: &OpenFile, at: u64) {
    if let Ok(at) = i64::try_from(at) {
        let _ = file.seek(at, Whence::Set);
    }
}
