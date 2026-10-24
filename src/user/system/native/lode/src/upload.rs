//! `lode upload-stub PORT FILE`: a collector for tests (`docs/LOGGING.md`
//! §10.2). It keeps what `lode-upload` sends it over plain HTTP on
//! 127.0.0.1, and answers 503 while told to, so `test-log` can take a
//! collector away and bring it back. The uploader itself is `lode-upload`
//! (`src/user/system/linux/lode-net`).

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::blob::BlobId;
use ferrix_lode_core::upload;

use crate::{cli, sys};

/// `lode upload-stub PORT FILE`: a collector on `127.0.0.1:PORT` that
/// appends each request's body to FILE and answers 200 -- or 503 while
/// `FILE.refuse` exists, keeping nothing. A blob (`docs/LOGGING.md`
/// §21.4) is checked against its id, 400 when it is not the id's, and
/// said as a line `BLOB <id> <type> <size>` in `FILE.blobs`. A test peer.
pub(crate) fn stub(words: &[String]) -> i32 {
    let [port, file] = words else {
        return cli::usage("upload-stub PORT FILE");
    };
    let Ok(port) = port.parse::<u16>() else {
        return cli::usage("upload-stub PORT FILE");
    };
    let listener = match sys::tcp_listen(port) {
        Ok(fd) => fd,
        Err(error) => {
            cli::complain(&alloc::format!(
                "lode: upload-stub: port {port}: errno {}",
                error.0
            ));
            return 1;
        }
    };
    cli::out(alloc::format!("upload-stub: listening on 127.0.0.1:{port}\n").as_bytes());
    let refuse = alloc::format!("{file}.refuse").into_bytes();
    loop {
        let Ok(fd) = sys::accept_blocking(listener) else {
            continue;
        };
        let mut request = Vec::new();
        let mut chunk = [0_u8; 8192];
        let whole = loop {
            if let Some((start, length)) = upload::request_head(&request)
                && request.len() >= start + length
            {
                break split(&request, start, length);
            }
            match sys::read(fd, &mut chunk) {
                Ok(0) | Err(_) => break None,
                Ok(got) => request.extend_from_slice(chunk.get(..got).unwrap_or_default()),
            }
        };
        let answer: &[u8] = match whole {
            Some(_) if sys::read_small(&refuse).is_some() => {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
            }
            Some((Some(Err(_)), _)) => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
            Some((blob, body)) => {
                let (path, kept_bytes) = match blob {
                    Some(Ok((id, kind))) => (
                        alloc::format!("{file}.blobs"),
                        alloc::format!(
                            "BLOB {} {kind} {}\n",
                            ferrix_lode_core::blob::hex(&id),
                            body.len()
                        )
                        .into_bytes(),
                    ),
                    _ => (file.clone(), body),
                };
                let body = kept_bytes;
                let kept = sys::openat(
                    sys::AT_FDCWD,
                    path.as_bytes(),
                    sys::O_WRONLY | sys::O_CREAT | sys::O_APPEND | sys::O_CLOEXEC,
                    0o644,
                )
                .and_then(|out| {
                    let written = sys::write_all(out, &body);
                    sys::close(out);
                    written
                });
                if kept.is_ok() {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                } else {
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
                }
            }
            None => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
        };
        let _ = sys::write_all(fd, answer);
        sys::close(fd);
    }
}

/// A whole request's body, and for a blob what its head says of it, its
/// bytes checked against its id.
type Taken = (Option<Result<(BlobId, String), String>>, Vec<u8>);

/// The body of `request`, whose head ends at `start`, `length` bytes.
fn split(request: &[u8], start: usize, length: usize) -> Option<Taken> {
    let head = request.get(..start)?;
    let body = Vec::from(request.get(start..start + length)?);
    let blob = upload::header(head, "Content-Type")
        .is_some_and(|kind| kind.starts_with(upload::BLOB_CONTENT_TYPE));
    let checked = blob.then(|| upload::parse_blob(head, &body));
    Some((checked, body))
}
