//! Core quarantine's door (`docs/CORE-QUARANTINE.md` §4, §5): the processor
//! service token init grants lode at every start, and the call that takes a
//! processor out of service through it.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::ingest::{self, Now};
use ferrix_lode_core::mce;
use ferrix_lode_core::quarantine::{
    Config, Judge, MESSAGE_ID_QUARANTINE, Verdict, brought_back_by_root, core_scoped, cpu_list,
};
use ferrix_lode_core::record::Record;
use ferrix_native_abi::directory::{self, Kind, MAX_MESSAGE, Message};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::{Handle, OwnedHandle, Requested, Rights, pending};

use crate::sys;

/// The token, with `MANAGE` alone.
pub(crate) struct Token(OwnedHandle<Kernel>);

impl Token {
    /// Take the token from init's GRANT, which waits first on `channel`
    /// when init has one to give: written before lode started, so it is
    /// read without waiting. `TRANSFER` is dropped at once -- a handle
    /// travels with its rights, so init could not send it without -- and
    /// lode never sends it on. `None` when init granted none.
    ///
    /// Nothing else can be first on the channel, so nothing else is taken
    /// here: init writes a GRANT before lode starts or nothing, and every
    /// other message init sends is an answer to one lode has not yet sent
    /// (CONNECT to an OFFER, REFUSED to an OPEN; `docs/INIT.md` §6).
    pub(crate) fn take(channel: &Channel<Kernel>) -> Option<Token> {
        let mut bytes = [0_u8; MAX_MESSAGE];
        let mut handles = [Handle::INVALID; 1];
        let received = channel.read(&mut bytes, &mut handles).ok()?;
        if received.handles != 1 {
            return None;
        }
        let handle = OwnedHandle::from_raw(Kernel, handles[0]);
        let message = Message::decode(bytes.get(..received.bytes).unwrap_or_default())?;
        if message.kind != Kind::Grant || message.name != directory::CPU_SERVICE {
            return None;
        }
        handle
            .replace(Requested::Exactly(Rights::MANAGE))
            .ok()
            .map(Token)
    }

    /// Take processor `cpu` out of service.
    pub(crate) fn take_out(&self, cpu: usize) -> Result<(), ferrix_rt::native::Error> {
        pending::cpu_service(Kernel, &self.0, cpu, false)
    }
}

/// The policy at work: the judge, and the token it acts through.
pub(crate) struct Quarantine {
    judge: Judge,
    token: Option<Token>,
}

impl Quarantine {
    /// The policy `config` says, acting through `token`. Processors already
    /// out of service -- an earlier lode of this boot took them -- count
    /// against the cap.
    pub(crate) fn new(config: Config, token: Option<Token>) -> Quarantine {
        let mut judge = Judge::new(config);
        let (in_service, cpus) = counts();
        judge.took_out(cpus.saturating_sub(in_service));
        Quarantine { judge, token }
    }

    /// A kernel line read: a machine check of a core's own counts against
    /// that core, and root's return of a processor starts its core again.
    /// What lode did is said in `said`.
    pub(crate) fn heard(&mut self, text: &[u8], now: Now, said: &mut Vec<Record>) {
        if let Some(cpu) = brought_back_by_root(text) {
            let (core, _) = core_of(cpu);
            self.judge.returned(core, 1);
            return;
        }
        let Some(line) = mce::read(text) else {
            return;
        };
        if !core_scoped(&line) {
            return;
        }
        let Ok(cpu) = line.cpu.parse::<usize>() else {
            return;
        };
        let (core, threads) = core_of(cpu);
        let config = *self.judge.config();
        let seconds = config.window_us / 1_000_000;
        let enough = alloc::format!(
            "processor {cpu}'s core (processors {}) had {} machine checks of its own in {seconds} s",
            list(&threads),
            config.count
        );
        match self.judge.count(core, now.monotonic_us) {
            Verdict::Counted => {}
            Verdict::Warn => say(said, 4, &alloc::format!("{enough}; action is warn"), now),
            Verdict::TakeOut => self.take_out(core, &threads, &enough, now, said),
        }
    }

    /// Take `core`'s processors out, within the bounds, and say what came
    /// of each: a partial outcome (one sibling refused) is said as such.
    fn take_out(
        &mut self,
        core: u64,
        threads: &[usize],
        enough: &str,
        now: Now,
        said: &mut Vec<Record>,
    ) {
        let (in_service, cpus) = counts();
        let Some(token) = &self.token else {
            say(
                said,
                3,
                &alloc::format!("{enough}; lode holds no token to take it out"),
                now,
            );
            return;
        };
        if !self.judge.allows(threads.len(), in_service, cpus) {
            if self.judge.say_refusal(core, now.monotonic_us) {
                say(
                    said,
                    3,
                    &alloc::format!("{enough}; left in service: past the cap or the keep"),
                    now,
                );
            }
            return;
        }
        let mut out = Vec::new();
        let mut refused = Vec::new();
        for &cpu in threads {
            match token.take_out(cpu) {
                Ok(()) => out.push(cpu),
                Err(error) => refused.push(alloc::format!("{cpu}: {error:?}")),
            }
        }
        self.judge.took_out(out.len());
        let text = if refused.is_empty() {
            alloc::format!("{enough}; taken out of service")
        } else {
            alloc::format!(
                "{enough}; taken out of service: {}, refused: {}",
                list(&out),
                refused.join(", ")
            )
        };
        say(said, 2, &text, now);
    }
}

/// Say once, at start, that init granted no token: no core will be
/// quarantined (a build without root's way back, `docs/CORE-QUARANTINE.md`
/// §4).
pub(crate) fn say_if_ungranted(token: Option<&Token>, said: &mut Vec<Record>, now: Now) {
    if token.is_none() {
        say(
            said,
            4,
            "lode was granted no processor service token: no core will be quarantined",
            now,
        );
    }
}

/// Say `text` at `priority`, as lode.
fn say(said: &mut Vec<Record>, priority: u8, text: &str, now: Now) {
    said.push(ingest::own(priority, MESSAGE_ID_QUARANTINE, text, now));
}

/// Processors as a comma list.
fn list(cpus: &[usize]) -> String {
    let words: Vec<String> = cpus.iter().map(|cpu| alloc::format!("{cpu}")).collect();
    words.join(",")
}

/// Processor `cpu`'s physical core, as its first thread, and its threads,
/// from `cpu<n>/topology/thread_siblings_list`; the processor alone where
/// the file cannot be read.
fn core_of(cpu: usize) -> (u64, Vec<usize>) {
    let path = alloc::format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    let threads = read_text(path.as_bytes())
        .and_then(|text| cpu_list(&text))
        .filter(|threads| threads.contains(&cpu))
        .unwrap_or_else(|| alloc::vec![cpu]);
    (threads.first().copied().unwrap_or(cpu) as u64, threads)
}

/// Processors in service now, and processors there are.
fn counts() -> (usize, usize) {
    let count = |path: &[u8]| {
        read_text(path)
            .and_then(|text| cpu_list(&text))
            .map_or(0, |cpus| cpus.len())
    };
    let present = count(b"/sys/devices/system/cpu/present");
    let online = count(b"/sys/devices/system/cpu/online");
    (online, present.max(online))
}

/// A small sysfs file's text.
fn read_text(path: &[u8]) -> Option<String> {
    let fd = sys::open(path, sys::O_RDONLY).ok()?;
    let mut buf = [0_u8; 256];
    let got = sys::read(fd, &mut buf);
    sys::close(fd);
    let bytes = buf.get(..got.ok()?)?;
    core::str::from_utf8(bytes).ok().map(String::from)
}
