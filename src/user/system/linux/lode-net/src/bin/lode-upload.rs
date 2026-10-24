//! `lode-upload`: a Ferrix machine's log sent on to collectors
//! (`docs/LOGGING.md` §10.2), as `lode-upload.service`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(u8::try_from(lode_net::upload::run(&args)).unwrap_or(1))
}
