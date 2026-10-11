//! The greeter: a service that answers every connection to its port with one
//! line and closes it.

use std::io::Write;
use std::net::{Ipv4Addr, TcpListener};

/// Where it listens, on the loopback.
const PORT: u16 = 7700;

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, PORT))?;
    println!(
        "greeter: listening on port {PORT}, saying {}",
        greeter::WORDS
    );
    for stream in listener.incoming() {
        let mut stream = stream?;
        writeln!(stream, "{}", greeter::greeting())?;
    }
    Ok(())
}
