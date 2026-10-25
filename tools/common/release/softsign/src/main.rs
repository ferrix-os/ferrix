//! `ferrix-softsign`: the development soft signer's command line
//! (`docs/SIGNING.md` §6.4). Development only; never a release.
//!
//! ```text
//! ferrix-softsign keygen  <label> [--height 10|15|20]
//! ferrix-softsign pubkey  <label>
//! ferrix-softsign remaining <label>
//! ferrix-softsign sign    <label> --kind slot|config|driver|revocation
//!                         --anchor-id <0x8000..> --profile <n> --arch <n>
//!                         --family <n> --security-version <n>
//!                         --payload <file> --out <file>
//!                         [--subject <text>] [--label <text>] [--release <n>]
//!                         [--floors a,b,c,d] [--target-kind <k>]
//!                         [--revoked <hex I>]...
//! common: [--dir <keys dir>] [--release-anchors <file of hex keys>]
//!
//! The CI development key (`docs/SIGNING.md` §6.5), with no state file:
//! ferrix-softsign ci-keygen --secret-out <file>   (prints the public key)
//! ferrix-softsign ci-range  --run-number <n> --run-attempt <a>
//! ferrix-softsign ci-sign   --run-number <n> --run-attempt <a>
//!                           --prior-logs <file> --log <file>
//!                           --anchors <signing-anchors.toml> --workflow <path>
//!                           <sign's manifest flags> --payload <file>
//!                           --out <file> [--into <slot image>]
//!                           (the key from FERRIX_CI_SIGNING_KEY)
//! ferrix-softsign ci-audit  <file of logs>
//! ```

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool: its answers and refusals are its output"
)]

#[cfg(test)]
use ferrix_sigverify as _;
use std::path::PathBuf;
use std::process::ExitCode;

use ferrix_softsign::ci;
use ferrix_softsign::lms::{
    LMOTS_SHA256_N24_W4, LMS_SHA256_M24_H10, LMS_SHA256_M24_H15, LMS_SHA256_M24_H20,
};
use ferrix_softsign::manifest::{self, Body};
use ferrix_softsign::token::{
    CKA_HSS_KEYS_REMAINING, CKA_VALUE, CKM_HSS_KEY_PAIR_GEN, ObjectHandle, SoftToken, Token,
};

/// The first anchor id a development key may carry (D1).
const DEVELOPMENT_ANCHOR_MIN: u16 = 0x8000;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn number(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("not a number: {text}"))
}

fn padded(text: &str) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    if text.len() > 32 {
        return Err(format!("longer than 32 bytes: {text}"));
    }
    for (o, b) in out.iter_mut().zip(text.bytes()) {
        *o = b;
    }
    Ok(out)
}

struct Args {
    words: Vec<String>,
    flags: Vec<(String, String)>,
}

impl Args {
    fn parse(raw: Vec<String>) -> Result<Args, String> {
        let mut words = Vec::new();
        let mut flags = Vec::new();
        let mut it = raw.into_iter();
        while let Some(arg) = it.next() {
            if let Some(name) = arg.strip_prefix("--") {
                let value = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
                flags.push((name.to_owned(), value));
            } else {
                words.push(arg);
            }
        }
        Ok(Args { words, flags })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> impl Iterator<Item = &str> {
        self.flags
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }
}

fn token(args: &Args) -> Result<SoftToken, String> {
    let dir = match args.get("dir") {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
            PathBuf::from(home).join(".local/share/ferrix/signing-dev")
        }
    };
    let mut release = Vec::new();
    if let Some(file) = args.get("release-anchors") {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let bytes: Result<Vec<u8>, String> = (0..line.len() / 2)
                .map(|n| {
                    line.get(2 * n..2 * n + 2)
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                        .ok_or_else(|| format!("{file}: not hex: {line}"))
                })
                .collect();
            release.push(bytes?);
        }
    }
    SoftToken::open(&dir, release).map_err(|e| e.to_string())
}

/// The CI key's commands (`docs/SIGNING.md` §6.5); `None` when `command`
/// is none of them.
fn ci(command: &str, args: &Args) -> Option<Result<(), String>> {
    // The flags' numbers, through the module's `number` (a closure named
    // the same inside another would call itself: the first CI run's
    // "--1 is required").
    let flag = |name: &str| args.need(name).and_then(number);
    let run = |args: &Args| -> Result<std::ops::Range<u32>, String> {
        let flag = |name: &str| args.need(name).and_then(number);
        ci::leaf_range(flag("run-number")?, flag("run-attempt")?, 1 << 20)
            .map_err(|e| e.to_string())
    };
    Some(match command {
        "ci-range" => run(args).map(|range| println!("{} {}", range.start, range.end)),
        "ci-keygen" => ci_keygen(args),
        "ci-audit" => (|| {
            let file = args.words.get(1).ok_or("ci-audit <file of logs>")?;
            let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
            let entries = ci::audit(&text).map_err(|e| e.to_string())?;
            println!("{entries} signature(s), no (I, q) twice");
            Ok(())
        })(),
        "ci-sign" => (|| {
            let range = run(args)?;
            let secret = std::env::var("FERRIX_CI_SIGNING_KEY")
                .map_err(|_| "FERRIX_CI_SIGNING_KEY is not set".to_owned())?;
            let key = ci::parse_secret(&secret).map_err(|e| e.to_string())?;
            drop(secret);
            let prior = match args.get("prior-logs") {
                Some(file) => std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?,
                None => {
                    return Err(
                        "--prior-logs is required: the logs of every earlier run".to_owned()
                    );
                }
            };
            let log = PathBuf::from(args.need("log")?);
            // The anchors file is required (ledger 4188 (4), (5)): its
            // release keys are refused (D1), its CI anchor must hold this
            // key, and an expired key signs nothing.
            let body = body(args)?;
            let (release, anchor) =
                ci::anchors_from(args.get("anchors"), body.anchor_id).map_err(|e| e.to_string())?;
            ci::refuse_expired(&anchor, &ci::today()).map_err(|e| e.to_string())?;
            let context = ci::RunContext {
                run_number: flag("run-number")?,
                run_attempt: flag("run-attempt")?,
                workflow: args.need("workflow")?.to_owned(),
            };
            let mut signer = ci::Signer::open(key, range, &prior, &log, &release, &context)
                .map_err(|e| e.to_string())?;
            if signer.public_key() != anchor.public_key {
                return Err(ci::CiError::WrongKey.to_string());
            }
            let bytes = body.encode();
            let signature = signer.sign(&bytes).map_err(|e| e.to_string())?;
            let block = manifest::assemble(&bytes, &signature);
            let out = args.need("out")?;
            std::fs::write(out, &block).map_err(|e| format!("{out}: {e}"))?;
            if let Some(image) = args.get("into") {
                let mut slot = std::fs::read(image).map_err(|e| format!("{image}: {e}"))?;
                slot.get_mut(4096..8192)
                    .ok_or_else(|| format!("{image}: shorter than its manifest block"))?
                    .copy_from_slice(&block);
                std::fs::write(image, slot).map_err(|e| format!("{image}: {e}"))?;
            }
            Ok(())
        })(),
        _ => return None,
    })
}

/// `ci-keygen`: a fresh H20 key, its secret text written to a new file only
/// its owner may read, outside any git work tree; the public key printed.
fn ci_keygen(args: &Args) -> Result<(), String> {
    use std::io::{Read as _, Write as _};
    let out = PathBuf::from(args.need("secret-out")?);
    let parent = out
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut at = Some(parent.as_path());
    while let Some(dir) = at {
        if dir.join(".git").exists() {
            return Err(format!("{}: inside a git work tree (D5)", out.display()));
        }
        at = dir.parent();
    }
    let mut random = [0u8; 16 + ferrix_softsign::lms::N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    let (id, seed) = random.split_at(16);
    let key = ferrix_softsign::lms::Key {
        lms_type: LMS_SHA256_M24_H20,
        id: id.try_into().map_err(|_| "id")?,
        seed: seed.try_into().map_err(|_| "seed")?,
    };
    let mut options = std::fs::OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = options.mode(0o600);
    }
    let mut file = options
        .open(&out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    file.write_all(ci::render_secret(&key).as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let context = ci::RunContext {
        run_number: 0,
        run_attempt: 0,
        workflow: String::new(),
    };
    let signer = ci::Signer::open(key, 0..0, "", &out, &[], &context).map_err(|e| e.to_string())?;
    println!("{}", hex(&signer.public_key()));
    Ok(())
}

fn run(raw: Vec<String>) -> Result<(), String> {
    let args = Args::parse(raw)?;
    if let Some(command) = args.words.first()
        && let Some(done) = ci(command, &args)
    {
        return done;
    }
    let (command, label) = match args.words.as_slice() {
        [command, label] => (command.as_str(), label.clone()),
        _ => {
            return Err(
                "usage: ferrix-softsign keygen|pubkey|remaining|sign <label> [options]".to_owned(),
            );
        }
    };
    let mut token = token(&args)?;
    let key = ObjectHandle(label.clone());
    match command {
        "keygen" => {
            let lms_type = match args.get("height").unwrap_or("10") {
                "10" => LMS_SHA256_M24_H10,
                "15" => LMS_SHA256_M24_H15,
                "20" => LMS_SHA256_M24_H20,
                other => return Err(format!("--height {other}: 10, 15 or 20")),
            };
            let _ = token
                .generate_key_pair(
                    CKM_HSS_KEY_PAIR_GEN,
                    &label,
                    1,
                    lms_type,
                    LMOTS_SHA256_N24_W4,
                )
                .map_err(|e| e.to_string())?;
            let public = token
                .get_attribute(&key, CKA_VALUE)
                .map_err(|e| e.to_string())?;
            println!("{}", hex(&public));
        }
        "pubkey" => {
            let public = token
                .get_attribute(&key, CKA_VALUE)
                .map_err(|e| e.to_string())?;
            println!("{}", hex(&public));
        }
        "remaining" => {
            let left = token
                .get_attribute(&key, CKA_HSS_KEYS_REMAINING)
                .map_err(|e| e.to_string())?;
            let bytes: [u8; 8] = left
                .try_into()
                .map_err(|_| "a malformed answer".to_owned())?;
            println!("{}", u64::from_be_bytes(bytes));
        }
        "sign" => {
            let body = body(&args)?;
            let block = manifest::sign(&mut token, &key, &body).map_err(|e| e.to_string())?;
            let out = args.need("out")?;
            std::fs::write(out, block).map_err(|e| format!("{out}: {e}"))?;
        }
        other => return Err(format!("unknown command {other}")),
    }
    Ok(())
}

fn body(args: &Args) -> Result<Body, String> {
    let kind = match args.need("kind")? {
        "slot" => 1,
        "config" => 2,
        "driver" => 3,
        "revocation" => 4,
        other => return Err(format!("--kind {other}")),
    };
    let anchor_id =
        u16::try_from(number(args.need("anchor-id")?)?).map_err(|_| "--anchor-id past u16")?;
    if anchor_id < DEVELOPMENT_ANCHOR_MIN {
        return Err(format!(
            "--anchor-id {anchor_id:#x}: development keys use {DEVELOPMENT_ANCHOR_MIN:#x} and up; product ids are never the soft signer's"
        ));
    }
    let payload = std::fs::read(args.need("payload")?).map_err(|e| format!("--payload: {e}"))?;
    let mut floors = [0u32; 4];
    if let Some(list) = args.get("floors") {
        for (slot, word) in floors.iter_mut().zip(list.split(',')) {
            *slot = u32::try_from(number(word)?).map_err(|_| "--floors past u32")?;
        }
    }
    let mut revoked = Vec::new();
    for id in args.all("revoked") {
        let mut out = [0u8; 16];
        for (n, slot) in out.iter_mut().enumerate() {
            *slot = id
                .get(2 * n..2 * n + 2)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("--revoked {id}: 32 hex digits"))?;
        }
        revoked.push(out);
    }
    Ok(Body {
        kind,
        security_version: u32::try_from(number(args.need("security-version")?)?)
            .map_err(|_| "past u32")?,
        anchor_id,
        profile: u8::try_from(number(args.need("profile")?)?).map_err(|_| "--profile past u8")?,
        arch: u8::try_from(number(args.need("arch")?)?).map_err(|_| "--arch past u8")?,
        family: u32::try_from(number(args.need("family")?)?).map_err(|_| "--family past u32")?,
        release: args.get("release").map(number).transpose()?.unwrap_or(0),
        label: padded(args.get("label").unwrap_or(""))?,
        subject: padded(args.get("subject").unwrap_or(""))?,
        payload_len: u64::try_from(payload.len()).map_err(|_| "payload too long")?,
        payload_sha256: ferrix_sha256::digest(&payload),
        floors,
        target_kind: args
            .get("target-kind")
            .map(number)
            .transpose()?
            .map_or(Ok(0), u16::try_from)
            .map_err(|_| "--target-kind past u16")?,
        revoked,
    })
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("ferrix-softsign: {why}");
            ExitCode::FAILURE
        }
    }
}
