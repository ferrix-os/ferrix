//! lode, the log manager (`docs/LOGGING.md`).
//!
//! One program, two roles, chosen by how it was started:
//!
//! * by init, as `lode.service` (`Type=native`, in a sealed job), with a
//!   bootstrap channel: the service ([`serve`]), which reads the kernel log
//!   and keeps it in the store;
//! * from a shell, by `execve`, with none: the reader ([`cli`]), `lode` or
//!   `journalctl` through a link, and `lode mark` and `lode verify`.
//!
//! The exit status of the service names the step that stopped it
//! ([`serve::Step`]); 0 is init closing its channel.

#![no_std]
#![no_main]

extern crate alloc;

mod admin;
mod alerting;
mod arch;
mod blobs;
mod capture;
mod cli;
mod control;
mod crashes;
mod front;
mod handout;
mod heap;
mod seat;
mod serve;
mod snapshot;
mod store;
mod sys;
mod top;
mod units;
mod upload;
mod zone;

use ferrix_rt::Bootstrap;

ferrix_rt::entry!(main);

fn main(bootstrap: Bootstrap) -> i32 {
    match bootstrap {
        Some(channel) => match serve::run(&channel) {
            Ok(()) => 0,
            Err(step) => {
                let _ = sys::write_all(
                    2,
                    alloc::format!("lode: the service stopped at {step:?}\n").as_bytes(),
                );
                step as i32
            }
        },
        None => cli::run(),
    }
}
