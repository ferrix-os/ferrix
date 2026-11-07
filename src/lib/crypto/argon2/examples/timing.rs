//! `timing [m_kib t]...`: how long one Argon2id check takes here, for the
//! parameters `authd` would choose (`docs/AUTH.md` §5.1).
//!
//! With no arguments it times the floor (19 MiB, two passes) and RFC 9106's
//! second recommendation with one lane (64 MiB, three passes). Each is run
//! three times and the fastest is printed, since what is wanted is the cost
//! of the arithmetic and not of whatever else the machine was doing.

use std::io::Write as _;
use std::time::Instant;

use ferrix_argon2::{Block, Inputs, Params, hash};

fn main() {
    let words: Vec<u32> = std::env::args()
        .skip(1)
        .filter_map(|word| word.parse().ok())
        .collect();
    let pairs: Vec<(u32, u32)> = if words.len() >= 2 {
        words
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|pair| Some((*pair.first()?, *pair.get(1)?)))
            .collect()
    } else {
        vec![(19_456, 2), (65_536, 3)]
    };
    for (memory_kib, passes) in pairs {
        let params = Params {
            memory_kib,
            passes,
            lanes: 1,
        };
        let Ok(blocks) = params.blocks() else {
            say(&format!("m={memory_kib} t={passes}: not allowed"));
            continue;
        };
        let mut memory = vec![Block::ZERO; blocks];
        let mut out = [0_u8; 32];
        let inputs = Inputs {
            password: b"a password of ordinary length",
            salt: b"0123456789abcdef",
            secret: &[],
            associated: &[],
        };
        let mut best = f64::MAX;
        for _ in 0..3 {
            let start = Instant::now();
            if hash(&params, &inputs, &mut memory, &mut out).is_err() {
                say(&format!("m={memory_kib} t={passes}: refused"));
                break;
            }
            best = best.min(start.elapsed().as_secs_f64());
        }
        say(&format!(
            "m={memory_kib} t={passes} p=1: {:.0} ms",
            best * 1000.0
        ));
    }
}

/// Write one line to standard output.
fn say(line: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}
