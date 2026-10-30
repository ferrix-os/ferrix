//! Capabilities and the directory (`docs/INIT.md` §6, landing L8's init
//! side): what init does with the native calls K2 to K6 made.
//!
//! * Pid 1's own bootstrap channel ([`Directory::open`]): taken with
//!   `process_bootstrap`, and the kernel's hello read from it (K2). Init
//!   keeps it; later versions carry handles over it.
//! * One port, as a descriptor in init's epoll (`port_fd`, K4): every
//!   service channel and every native process is watched through it.
//! * A bootstrap channel for each service that declares `Uses=` or
//!   `Offers=`, and for every `Type=native` one. A Linux service is given its
//!   end with `process_give` (K3) while it waits, before `execve`, on a pipe
//!   init writes once the give is done (§5.2 step 3); a native one gets it
//!   from `process_start`.
//! * `Type=native` services, made with `process_create` in the job behind
//!   their cgroup (`job_for_cgroup`, C8) from the ELF file `ExecStart=`
//!   names, and watched for `TERMINATED`; how one ended is read with
//!   `process_status` (K6).
//! * The directory's messages on those channels ([`directory`]): READY
//!   is readiness, OFFER a provider's channel for a name, OPEN a client's
//!   request, which the manager routes ([`Action::Route`]) or refuses
//!   ([`Action::Refuse`]); init forwards the client's end as CONNECT, or
//!   answers REFUSED.
//!
//! [`directory`]: ferrix_native_abi::directory
//! [`Action::Route`]: ferrix_svc::event::Action::Route
//! [`Action::Refuse`]: ferrix_svc::event::Action::Refuse

use std::collections::BTreeMap;
use std::os::fd::{BorrowedFd, RawFd};

use ferrix_native::channel::{self, Channel, ReadError};
use ferrix_native::job::{self, Job};
use ferrix_native::pending::{self, Process};
use ferrix_native::port::{self, Port};
use ferrix_native::{
    Deadline, Error, Handle, Object, OwnedHandle, Requested, Rights, Signals, vmo,
};
use ferrix_native_abi::bootstrap::{
    AUDIT_MAGIC, CPU_SERVICE_MAGIC, DEVMGR_STARTER_MAGIC, MEMORY_SCRUB_MAGIC, ROOT_MAGIC,
    ROOT_SWITCHED, init_hello_version, read_after_hello,
};
use ferrix_native_abi::directory::{self as wire, Kind, MAX_MESSAGE, Message};
use ferrix_native_abi::types::{PACKET_SIGNAL, PROCESS_EXITED, PROCESS_KILLED};
use ferrix_svc::event::{Event, Exit, Name, Pid, Token, UnitId};
use ferrix_svc::value::Signal;

use crate::spawn::Ids;
use crate::sys::{self, Forked, Native};

/// What a port key stands for.
#[derive(Debug)]
enum Watched {
    /// A service's bootstrap channel: init's end.
    Channel(UnitId, Channel<Native>),
    /// A native service's process.
    Process(UnitId, u32, Process<Native>),
}

/// A client's end in transit, from its OPEN to the provider's CONNECT.
#[derive(Debug)]
struct End {
    from: UnitId,
    handle: OwnedHandle<Native>,
}

/// The directory's state.
#[derive(Debug)]
pub(crate) struct Directory {
    /// Pid 1's own channel: the hello, then under `ferrix.devmgr=init`
    /// devmgr's starter and, later, where `/` is (§7.3, L12).
    own: Option<Channel<Native>>,
    /// The starter the kernel gave pid 1, with which it asks the kernel to
    /// start `devmgr`.
    starter: Option<OwnedHandle<Native>>,
    /// The audit record's handle, with `READ` alone, until the reader takes
    /// it (`docs/certification/AUDIT.md` §4).
    audit: Option<OwnedHandle<Native>>,
    /// The processor service token, kept: lode is granted a copy at every
    /// start (`docs/CORE-QUARANTINE.md` §4).
    cpu_service: Option<OwnedHandle<Native>>,
    /// The memory scrub token, kept: memscrub is granted a copy at every
    /// start (`docs/RAM-SCRUBBER.md` §4).
    memory_scrub: Option<OwnedHandle<Native>>,
    /// What the kernel said about `/`, until the caller takes it: whether
    /// it is the root volume now.
    root: Option<bool>,
    port: Port<Native>,
    /// What each port key stands for.
    watched: BTreeMap<u64, Watched>,
    /// Each unit's bootstrap channel, by its port key.
    channel_of: BTreeMap<UnitId, u64>,
    /// The channel ends providers offered, by name.
    offers: BTreeMap<String, Channel<Native>>,
    /// Client ends waiting for their route.
    ends: BTreeMap<u64, End>,
    next: u64,
}

/// The port key pid 1's own channel is watched under; the others count up
/// from 1.
const OWN: u64 = u64::MAX;

/// A spawn's native half, for the caller to finish.
#[derive(Debug)]
pub(crate) struct Given {
    /// The service's end, to give it.
    pub(crate) end: Channel<Native>,
}

impl Directory {
    /// Take pid 1's bootstrap channel and read the kernel's hello, and make
    /// the port. Returns the directory, the port's descriptor to watch, and
    /// a line saying what the kernel said.
    pub(crate) fn open() -> Result<(Directory, RawFd, String), Error> {
        let own = pending::take_bootstrap(Native)?.map(Channel::from_owned);
        let said = match &own {
            Some(channel) => {
                let mut bytes = [0_u8; 64];
                match channel.read(&mut bytes, &mut []) {
                    Ok(received) => match init_hello_version(
                        bytes.get(..received.bytes).unwrap_or_default(),
                    ) {
                        Some(version) => format!("the kernel greeted init, version {version}"),
                        None => {
                            "init's bootstrap channel held something that is not the kernel's hello"
                                .to_owned()
                        }
                    },
                    Err(_) => "init's bootstrap channel held no hello".to_owned(),
                }
            }
            None => "init was started with no bootstrap channel".to_owned(),
        };
        let port = port::create(Native)?;
        let fd = port.descriptor(true)?;
        let mut directory = Directory {
            own,
            starter: None,
            audit: None,
            cpu_service: None,
            memory_scrub: None,
            root: None,
            port,
            watched: BTreeMap::new(),
            channel_of: BTreeMap::new(),
            offers: BTreeMap::new(),
            ends: BTreeMap::new(),
            next: 1,
        };
        directory.read_own();
        if let Some(own) = &directory.own {
            let _ = own.wait_async(&directory.port, Signals::READABLE, OWN);
        }
        let said = match directory.starter {
            Some(_) => format!("{said}, and gave it devmgr's starter"),
            None => said,
        };
        Ok((directory, fd, said))
    }

    /// Read what waits on pid 1's own channel after the hello: devmgr's
    /// starter, the audit record's handle, and where `/` is.
    fn read_own(&mut self) {
        let Some(own) = &self.own else {
            return;
        };
        loop {
            let mut bytes = [0_u8; 16];
            let mut handles = [Handle(0); 1];
            let Ok(got) = own.read(&mut bytes, &mut handles) else {
                return;
            };
            let said = read_after_hello(bytes.get(..got.bytes).unwrap_or_default());
            let handle = (got.handles == 1).then(|| OwnedHandle::from_raw(Native, handles[0]));
            match (said, handle) {
                (Some((DEVMGR_STARTER_MAGIC, _)), Some(starter)) => self.starter = Some(starter),
                (Some((AUDIT_MAGIC, _)), Some(audit)) => self.audit = Some(audit),
                (Some((CPU_SERVICE_MAGIC, _)), Some(token)) => self.cpu_service = Some(token),
                (Some((MEMORY_SCRUB_MAGIC, _)), Some(token)) => self.memory_scrub = Some(token),
                (Some((ROOT_MAGIC, value)), None) => self.root = Some(value == ROOT_SWITCHED),
                _ => {}
            }
        }
    }

    /// The audit record's handle, if the kernel gave pid 1 one and nothing
    /// has taken it yet.
    pub(crate) fn take_audit(&mut self) -> Option<OwnedHandle<Native>> {
        self.audit.take()
    }

    /// GRANT `unit` a copy of the processor service token, with `MANAGE`
    /// and `TRANSFER` (`docs/CORE-QUARANTINE.md` §4): written into its new
    /// bootstrap channel before it starts, so it is the first thing there.
    /// init keeps its own, and grants again at every start.
    pub(crate) fn grant_cpu_service(&self, unit: UnitId) -> Result<(), String> {
        let token = self
            .cpu_service
            .as_ref()
            .ok_or_else(|| "the kernel gave init no processor service token".to_owned())?;
        self.grant(unit, token, wire::CPU_SERVICE)
    }

    /// GRANT `unit` a copy of the memory scrub token, as
    /// [`Directory::grant_cpu_service`] (`docs/RAM-SCRUBBER.md` §4).
    pub(crate) fn grant_memory_scrub(&self, unit: UnitId) -> Result<(), String> {
        let token = self
            .memory_scrub
            .as_ref()
            .ok_or_else(|| "the kernel gave init no memory scrub token".to_owned())?;
        self.grant(unit, token, wire::MEMORY_SCRUB)
    }

    /// GRANT `unit` a copy of `token` named `name`, with `MANAGE` and
    /// `TRANSFER`, into its new bootstrap channel.
    fn grant(&self, unit: UnitId, token: &OwnedHandle<Native>, name: &str) -> Result<(), String> {
        let copy = token
            .duplicate(Requested::Exactly(Rights::MANAGE | Rights::TRANSFER))
            .map_err(|error| format!("copying the {name} token: {error:?}"))?;
        let mut bytes = [0_u8; MAX_MESSAGE];
        let len = Message::named(Kind::Grant, name, "")
            .encode(&mut bytes)
            .ok_or_else(|| "a GRANT does not fit a message".to_owned())?;
        let channel = self
            .channel(unit)
            .ok_or_else(|| "it has no bootstrap channel".to_owned())?;
        channel
            .write_with(bytes.get(..len).unwrap_or_default(), [copy])
            .map_err(|(error, _)| format!("GRANT: {error:?}"))
    }

    /// Whether the kernel gave pid 1 devmgr's starter.
    pub(crate) fn has_starter(&self) -> bool {
        self.starter.is_some()
    }

    /// What the kernel said about `/` since the last call: `Some(true)` when
    /// it is the root volume now, with pid 1 on it.
    pub(crate) fn take_root(&mut self) -> Option<bool> {
        self.root.take()
    }

    /// Ask the kernel to start `devmgr` in the job behind `cgroup`, with the
    /// starter, and watch the process it answers as a native service's.
    pub(crate) fn start_devmgr(
        &mut self,
        unit: UnitId,
        cgroup: BorrowedFd<'_>,
    ) -> Result<u64, String> {
        use std::os::fd::AsRawFd as _;
        let starter = self
            .starter
            .as_ref()
            .ok_or_else(|| "no starter from the kernel: it starts devmgr itself".to_owned())?;
        let job: Job<Native> = job::for_cgroup(
            Native,
            cgroup.as_raw_fd(),
            Requested::Exactly(Rights::MANAGE),
        )
        .map_err(|error| format!("job_for_cgroup: {error:?}"))?;
        let process = pending::start_devmgr(starter, &job)
            .map_err(|error| format!("devmgr_start: {error:?}"))?;
        let key = self.key();
        process
            .notify_on_exit(&self.port, key)
            .map_err(|error| format!("waiting for its end: {error:?}"))?;
        let _ = self.watched.insert(key, Watched::Process(unit, 0, process));
        Ok(key)
    }

    fn key(&mut self) -> u64 {
        let key = self.next;
        self.next += 1;
        key
    }

    /// A new bootstrap channel for `unit`: init keeps and watches one end,
    /// and the other is returned to hand to the service.
    pub(crate) fn channel_for(&mut self, unit: UnitId) -> Result<Given, Error> {
        let (ours, theirs) = channel::create(Native)?;
        let key = self.key();
        ours.wait_async(&self.port, Signals::READABLE | Signals::PEER_CLOSED, key)?;
        if let Some(old) = self.channel_of.insert(unit, key) {
            let _ = self.watched.remove(&old);
        }
        let _ = self.watched.insert(key, Watched::Channel(unit, ours));
        Ok(Given { end: theirs })
    }

    /// `process_give`: hand a Linux child `end` before it runs its program.
    pub(crate) fn give(pid: u32, end: Channel<Native>) -> Result<(), Error> {
        pending::give_bootstrap(Native, pid, end.into_owned()).map_err(|(error, _)| error)
    }

    /// Start `program` as `unit`'s native main process, in the job behind
    /// the cgroup `cgroup` is open on, with `end` as its bootstrap channel.
    /// Returns its key, for the pid the caller finds in the cgroup.
    ///
    /// A native process runs as the process that made it (`docs/AUTH.md` §7,
    /// P0), so a unit with `ids` -- a `User=`, `Group=` or
    /// `SupplementaryGroups=` -- has its process made by a helper that has
    /// become them ([`made_by_helper`]); one without is made by init, and is
    /// root's.
    pub(crate) fn start_native(
        &mut self,
        unit: UnitId,
        cgroup: BorrowedFd<'_>,
        program: &str,
        end: Channel<Native>,
        ids: Option<&Ids>,
    ) -> Result<u64, String> {
        let process = match ids {
            Some(ids) => made_by_helper(cgroup, program, end, ids)?,
            None => make_native(cgroup, program, end)?,
        };
        let key = self.key();
        process
            .notify_on_exit(&self.port, key)
            .map_err(|error| format!("waiting for its end: {error:?}"))?;
        let _ = self.watched.insert(key, Watched::Process(unit, 0, process));
        Ok(key)
    }

    /// Record the pid a native process was found under in its cgroup.
    pub(crate) fn found_pid(&mut self, key: u64, pid: u32) {
        if let Some(Watched::Process(_, found, _)) = self.watched.get_mut(&key) {
            *found = pid;
        }
    }

    /// Drain the port: the events its packets are, and lines to say.
    pub(crate) fn drain(&mut self) -> (Vec<Event>, Vec<String>) {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        while let Ok(packet) = self.port.wait(Deadline::At(0)) {
            if packet.kind != PACKET_SIGNAL {
                continue;
            }
            if packet.key == OWN {
                self.read_own();
                if let Some(own) = &self.own {
                    let _ = own.wait_async(&self.port, Signals::READABLE, OWN);
                }
                continue;
            }
            match self.watched.remove(&packet.key) {
                Some(Watched::Channel(unit, channel)) => {
                    let open = self.read_channel(unit, &channel, &mut events, &mut lines);
                    if open
                        && channel
                            .wait_async(
                                &self.port,
                                Signals::READABLE | Signals::PEER_CLOSED,
                                packet.key,
                            )
                            .is_ok()
                    {
                        let _ = self
                            .watched
                            .insert(packet.key, Watched::Channel(unit, channel));
                    } else if self.channel_of.get(&unit) == Some(&packet.key) {
                        let _ = self.channel_of.remove(&unit);
                    }
                }
                Some(Watched::Process(unit, pid, process)) => {
                    let how = match process.status() {
                        Ok(status) if status.state == PROCESS_EXITED => {
                            Exit::Code(i32::try_from(status.value).unwrap_or(255))
                        }
                        Ok(status) if status.state == PROCESS_KILLED => Exit::Signal {
                            signal: Signal(u8::try_from(status.value).unwrap_or(9)),
                            core: false,
                        },
                        Ok(_) | Err(_) => Exit::Signal {
                            signal: Signal(9),
                            core: false,
                        },
                    };
                    let _ = unit;
                    events.push(Event::Exited { pid: Pid(pid), how });
                }
                None => {}
            }
        }
        (events, lines)
    }

    /// Read every message a unit's channel holds; whether it is still open.
    fn read_channel(
        &mut self,
        unit: UnitId,
        channel: &Channel<Native>,
        events: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) -> bool {
        loop {
            let mut bytes = [0_u8; MAX_MESSAGE];
            let mut handles = [Handle::INVALID; 1];
            let received = match channel.read(&mut bytes, &mut handles) {
                Ok(received) => received,
                Err(ReadError::Failed(Error::ShouldWait)) => return true,
                Err(ReadError::Failed(Error::PeerClosed)) => return false,
                Err(ReadError::TooSmall { .. }) => {
                    discard(channel);
                    lines.push("a service sent a message too big to be the directory's".to_owned());
                    continue;
                }
                Err(ReadError::Failed(_)) => return false,
            };
            let [handle] = handles;
            let handle = (received.handles == 1).then(|| OwnedHandle::from_raw(Native, handle));
            let Some(message) = Message::decode(bytes.get(..received.bytes).unwrap_or_default())
            else {
                lines.push("a service sent something that is not a directory message".to_owned());
                continue;
            };
            match (message.kind, handle) {
                (Kind::Ready, _) => events.push(Event::Ready { unit, status: None }),
                (Kind::Offer, Some(handle)) => {
                    let _ = self
                        .offers
                        .insert(message.name.to_owned(), Channel::from_owned(handle));
                }
                (Kind::Open, Some(handle)) => {
                    let token = self.key();
                    let _ = self.ends.insert(token, End { from: unit, handle });
                    events.push(Event::Open {
                        from: unit,
                        name: Name(message.name.to_owned()),
                        end: Token(token),
                    });
                }
                (kind, _) => lines.push(format!(
                    "a service sent {kind:?}, which is not a service's to send"
                )),
            }
        }
    }

    /// CONNECT: forward the client's end to the provider, down the channel
    /// it offered for the name, or else its bootstrap channel. `client` is
    /// the asking unit's name.
    pub(crate) fn route(
        &mut self,
        to: UnitId,
        name: &str,
        end: Token,
        client: impl Fn(UnitId) -> String,
    ) -> Result<(), String> {
        let End { from, handle } = self
            .ends
            .remove(&end.0)
            .ok_or_else(|| format!("no end {} to route for {name}", end.0))?;
        let detail = client(from);
        let mut bytes = [0_u8; MAX_MESSAGE];
        let len = Message::named(Kind::Connect, name, &detail)
            .encode(&mut bytes)
            .ok_or_else(|| format!("a CONNECT for {name} does not fit a message"))?;
        let bytes = bytes.get(..len).unwrap_or_default();
        let sent = match self.offers.get(name) {
            Some(offered) => offered.write_with(bytes, [handle]),
            None => match self.channel(to) {
                Some(channel) => channel.write_with(bytes, [handle]),
                None => return Err(format!("the provider of {name} has no channel")),
            },
        };
        sent.map_err(|(error, _)| format!("CONNECT for {name}: {error:?}"))
    }

    /// REFUSED: close the end and tell the asking unit why.
    pub(crate) fn refuse(&mut self, to: UnitId, name: &str, end: Token, why: &str) {
        let mut bytes = [0_u8; MAX_MESSAGE];
        if let (Some(len), Some(channel)) = (
            Message::named(Kind::Refused, name, why).encode(&mut bytes),
            self.channel(to),
        ) {
            let _ = channel.write(bytes.get(..len).unwrap_or_default());
        }
        // The end closes after REFUSED is sent, so a client that sees its
        // end close finds the reason already waiting.
        drop(self.ends.remove(&end.0));
    }

    /// A unit's bootstrap channel, while it is open.
    fn channel(&self, unit: UnitId) -> Option<&Channel<Native>> {
        let key = self.channel_of.get(&unit)?;
        match self.watched.get(key)? {
            Watched::Channel(_, channel) => Some(channel),
            Watched::Process(..) => None,
        }
    }

    /// A unit has stopped: close its channel, so its processes see
    /// `PEER_CLOSED`, and drop what it offered.
    pub(crate) fn forget(&mut self, unit: UnitId, offered: &[String]) {
        if let Some(key) = self.channel_of.remove(&unit) {
            let _ = self.watched.remove(&key);
        }
        for name in offered {
            let _ = self.offers.remove(name);
        }
    }
}

/// Take a message that is not the directory's off `channel`, and close the
/// handles it carried.
fn discard(channel: &Channel<Native>) {
    let mut bytes = vec![0_u8; 65536];
    let mut handles = [Handle::INVALID; 64];
    let Ok(got) = channel.read(&mut bytes, &mut handles) else {
        return;
    };
    for handle in handles.iter().take(got.handles) {
        drop(OwnedHandle::from_raw(Native, *handle));
    }
}

/// How long init waits for a helper to make and start a native service
/// (§5.2): reading the image as the user and loading it, on a loaded
/// emulator.
const HELPER_PATIENCE_NANOS: u64 = 10_000_000_000;

/// The helper's answer when the process was made and started: the process
/// handle comes with it. Anything else it writes is why it could not.
const STARTED: &[u8] = b"started";

/// Make and start `program` in the job behind `cgroup`, with `end` as its
/// bootstrap channel, as the caller runs: `job_for_cgroup` for `MANAGE`,
/// the image read into a VMO, `process_create` and `process_start`.
fn make_native(
    cgroup: BorrowedFd<'_>,
    program: &str,
    end: Channel<Native>,
) -> Result<Process<Native>, String> {
    use std::os::fd::AsRawFd as _;
    let image = std::fs::read(program).map_err(|error| format!("reading {program}: {error}"))?;
    let job: Job<Native> = job::for_cgroup(
        Native,
        cgroup.as_raw_fd(),
        Requested::Exactly(Rights::MANAGE),
    )
    .map_err(|error| format!("job_for_cgroup: {error:?}"))?;
    let elf = vmo::create(Native, image.len()).map_err(|error| format!("vmo_create: {error:?}"))?;
    elf.write(&image, 0)
        .map_err(|error| format!("vmo_write: {error:?}"))?;
    let name = program.rsplit('/').next().unwrap_or(program);
    let name = name
        .get(..name.len().min(ferrix_native_abi::nr::PROCESS_NAME_MAX))
        .unwrap_or(name);
    let process = pending::create_process(&job, &elf, name)
        .map_err(|error| format!("process_create: {error:?}"))?;
    process
        .start(end.into_owned())
        .map_err(|(error, _)| format!("process_start: {error:?}"))?;
    Ok(process)
}

/// [`make_native`] as `ids`: in a forked helper that becomes them first, so
/// the process it makes runs as them (P0).
///
/// Init makes a channel, writes `end` into its own side, and gives the
/// other to the helper with `process_give` (K3) before a pipe lets the
/// helper on, as it does for a Linux service's bootstrap. The helper takes
/// it, becomes the unit's user, reads `end`, makes and starts the process,
/// and writes a handle to it back, or why it could not; init waits up to
/// [`HELPER_PATIENCE_NANOS`] for that, and reaps the helper. The helper
/// gets `MANAGE` on the job through the cgroup's `cgroup.procs`, which the
/// caller has made the user's for the start.
fn made_by_helper(
    cgroup: BorrowedFd<'_>,
    program: &str,
    end: Channel<Native>,
    ids: &Ids,
) -> Result<Process<Native>, String> {
    let (ours, theirs) =
        channel::create(Native).map_err(|error| format!("a helper's channel: {error:?}"))?;
    ours.write_with(b"end", [end.into_owned()])
        .map_err(|(error, _)| format!("handing the helper the service's channel: {error:?}"))?;
    let (go_read, go_write) = sys::pipe().map_err(|error| format!("a helper's pipe: {error}"))?;
    let pid = match sys::fork().map_err(|error| format!("forking a helper: {error}"))? {
        Forked::Child => {
            drop(go_write);
            sys::wait_readable(std::os::fd::AsRawFd::as_raw_fd(&go_read));
            let status = helper(cgroup, program, ids);
            // SAFETY: `_exit` ends the helper without running the drops of
            // init's handles, whose numbers name nothing in its own table.
            unsafe { libc::_exit(status) }
        }
        Forked::Parent(pid) => pid,
    };
    drop(go_read);
    let given = pending::give_bootstrap(Native, pid, theirs.into_owned())
        .map_err(|(error, _)| format!("giving the helper its channel: {error:?}"));
    drop(go_write);
    let answer = given.and_then(|()| {
        let deadline = sys::monotonic().saturating_add(HELPER_PATIENCE_NANOS);
        let _ = ours
            .wait_one(
                Signals::READABLE | Signals::PEER_CLOSED,
                Deadline::At(deadline),
            )
            .map_err(|error| format!("the helper did not answer: {error:?}"))?;
        read_answer(&ours)
    });
    if answer.is_err() {
        let _ = sys::kill(pid, libc::SIGKILL);
    }
    let _ = sys::wait_for(pid, 500);
    answer
}

/// The helper's side of [`made_by_helper`]; its exit status.
fn helper(cgroup: BorrowedFd<'_>, program: &str, ids: &Ids) -> i32 {
    let Ok(Some(handle)) = pending::take_bootstrap(Native) else {
        return 2;
    };
    let reply = Channel::from_owned(handle);
    let (uid, gid, groups) = ids;
    let made = sys::become_user(*uid, *gid, groups)
        .map_err(|error| format!("becoming {uid}:{gid}: {error}"))
        .and_then(|()| {
            let mut bytes = [0_u8; 8];
            let mut handles = [Handle(0); 1];
            let got = reply
                .read(&mut bytes, &mut handles)
                .map_err(|error| format!("reading the service's channel: {error:?}"))?;
            if got.handles != 1 {
                return Err("init sent no channel for the service".to_owned());
            }
            let end = Channel::from_owned(OwnedHandle::from_raw(Native, handles[0]));
            make_native(cgroup, program, end)
        });
    let sent = match made {
        Ok(process) => reply.write_with(STARTED, [process.into_owned()]).is_ok(),
        Err(why) => reply.write(why.as_bytes()).is_ok(),
    };
    i32::from(!sent)
}

/// What a helper wrote back: the process it started, or why it did not.
fn read_answer(ours: &Channel<Native>) -> Result<Process<Native>, String> {
    let mut bytes = [0_u8; 256];
    let mut handles = [Handle(0); 1];
    let got = ours
        .read(&mut bytes, &mut handles)
        .map_err(|error| format!("the helper ended without an answer: {error:?}"))?;
    let said = bytes.get(..got.bytes).unwrap_or_default();
    match (said == STARTED, got.handles) {
        (true, 1) => Ok(Process::from_owned(OwnedHandle::from_raw(
            Native, handles[0],
        ))),
        _ => Err(String::from_utf8_lossy(said).into_owned()),
    }
}
