//! A test image of a safety partition (`docs/microkernel/BOOT-AUTHORITY.md` §6):
//! says it is up on the debug log its manager gave it, then ends. It ships in no
//! product image: xtask puts it only in `test-first-processes`' archive.

#![no_std]
#![no_main]

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::handle::{Deadline, Object as _};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(channel): Option<Channel<Kernel>> = bootstrap else {
        return 1;
    };
    if channel
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
        .is_err()
    {
        return 2;
    }
    let mut bytes = [0_u8; 8];
    let mut handles = [Handle::INVALID; 1];
    if channel.read(&mut bytes, &mut handles).is_err() {
        return 3;
    }
    ferrix_rt::log::use_debug_log(handles[0]);
    ferrix_rt::notice!("partition-hello  up in a safety partition");
    0
}
