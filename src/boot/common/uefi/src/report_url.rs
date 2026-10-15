//! Which `FERRIX/REPORT.TXT` the netbooted loader takes (`netreport.rs`).
//!
//! Pure and dependency-free, so that xtask's host tests compile this very
//! file (`tools/common/xtask/src/netboot/tests.rs`) and check every refusal.

/// The longest report URL taken.
pub(crate) const MAX_URL: usize = 256;

/// The URL in a `REPORT.TXT`, or why it is not one to use: one line of at
/// most [`MAX_URL`] bytes, `http://` and then printable ASCII with no space
/// (0x21 to 0x7e), ended by at most one line ending (`\n` or `\r\n`).
pub(crate) fn report_url(file: &[u8]) -> Result<&[u8], &'static str> {
    let url = file
        .strip_suffix(b"\r\n")
        .or_else(|| file.strip_suffix(b"\n"))
        .unwrap_or(file);
    if url.is_empty() {
        return Err("it is empty");
    }
    if url.len() > MAX_URL {
        return Err("it is longer than 256 bytes");
    }
    if !url.starts_with(b"http://") {
        return Err("it is not an http:// URL");
    }
    if !url.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
        return Err("it holds a byte that is not printable ASCII, or a space");
    }
    Ok(url)
}
