//! `--ssh <port>`: a watched boot you can log in to.
//!
//! `run-compositor --ssh 2222` forwards the host's `127.0.0.1:2222` to the
//! guest's port 22 and starts `sshdt` there from the compositor's own
//! `exec-once`, so `ssh -p 2222 root@127.0.0.1` reaches the guest as soon as
//! it is up. The server is the sshdt app (`cargo xtask build-apps --app
//! sshdt`); without it the forward leads nowhere and
//! the boot says so.
//!
//! # Who may log in
//!
//! Three sources, and the first is always there:
//!
//! 1. **This machine's guest key**, made once at
//!    `~/.local/share/ferrix/ssh/id_ed25519` and authorized by every boot.
//!    It is what a client with no key of its own logs in with, and the boot
//!    prints the `ssh -i` line that uses it.
//! 2. The keys that may already log in to the machine running QEMU: its
//!    user's `~/.ssh/authorized_keys`, and the public halves beside it in
//!    `~/.ssh`. This is what lets `remote-desktop` work without a key of its
//!    own: the key this PC logs in to the remote with is in the remote's
//!    `authorized_keys`, and so in the guest's.
//! 3. `--ssh-key <FILE|"ssh-… AAAA…">`, as many as given: a public key named
//!    on the command line, for a client whose key is in neither of those --
//!    another user on the machine, a sandbox, a CI step.
//!
//! The forward only listens on the QEMU host's loopback, so anybody who
//! reaches it is on that machine already; the keys make them prove which of
//! its users they are. Password authentication is never turned on: `sshdt`
//! given no key and no password accepts anyone, which is why source 1 exists
//! rather than a boot that starts no server.
//!
//! The guest key is generated, not borrowed, because a client that has no
//! private key cannot be helped by any number of public ones. Before it
//! existed, `ssh -p <port> root@127.0.0.1 <command>` from such a client
//! failed with `Permission denied (publickey)` -- which reads like a broken
//! session and is an empty `~/.ssh`.
//!
//! # The host key
//!
//! Kept on the machine running QEMU, at
//! `~/.local/share/ferrix/ssh/ssh_host_ed25519_key`, made with `ssh-keygen`
//! the first time. The initramfs is rebuilt for every boot, so a key the
//! guest made itself would be a new one each time, and `ssh` would refuse the
//! second boot for having changed its key.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::args::Args;
use crate::ports::{Content, File};
use crate::{Error, Result};

/// The port `sshdt` listens on in the guest.
pub(crate) const GUEST_PORT: u16 = 22;

/// Where the server is in the image, as `ports` installs it.
const SERVER_PATH: &str = "bin/sshdt";

/// Where the keys allowed to log in go in the image.
const KEYS_PATH: &str = "etc/ferrix/authorized_keys";

/// Where the host key goes in the image.
const HOST_KEY_PATH: &str = "etc/ferrix/ssh_host_ed25519_key";

/// `args` without `--ssh` when its port is already taken on this machine.
///
/// A `--forward` that cannot listen stops the run, because it was the point
/// of the run. `--ssh` on a desktop is a convenience beside the screen, and
/// `remote-desktop` asks for it on every boot, so a port somebody else holds
/// -- on the machine this was written on, a container publishing 2222 --
/// costs the SSH and says so, and the desktop comes up anyway.
pub(crate) fn checked(args: &Args) -> Args {
    let Some(port) = args.ssh else {
        return args.clone();
    };
    if std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok() {
        return args.clone();
    }
    println!("  ssh: 127.0.0.1:{port} is taken on this machine, so this boot has no SSH");
    println!("    --ssh <another port>, or [ssh] port for remote-desktop");
    Args {
        ssh: None,
        forwards: args
            .forwards
            .iter()
            .copied()
            .filter(|forward| (forward.host, forward.guest) != (port, GUEST_PORT))
            .collect(),
        ..args.clone()
    }
}

/// `config` with `sshdt` started, and what it needs added to `carried`, when
/// `--ssh` asked for it.
///
/// The server binds every address, because what it is reached through is the
/// gateway, which opens its connections to `10.0.2.15` and not to the
/// guest's loopback. It is started before the lease is taken, and that is
/// fine: a socket bound to `0.0.0.0` answers on an address that arrives
/// after it.
///
/// # Errors
///
/// A key file that exists and cannot be read, a `--ssh-key` that is neither a
/// readable file nor a key, or a key `ssh-keygen` could not make.
pub(crate) fn with_server(config: String, args: &Args, carried: &mut Vec<File>) -> Result<String> {
    let Some(port) = args.ssh else {
        return Ok(config);
    };
    if !carried.iter().any(|file| file.path == SERVER_PATH) {
        println!("  ssh: no sshdt in the image, so 127.0.0.1:{port} leads nowhere");
        println!("    `cargo xtask build-apps --app sshdt` builds it");
        return Ok(config);
    }
    let home = std::env::home_dir()
        .ok_or_else(|| Error::new("--ssh: no home directory to find the keys in"))?;
    let dir = guest_dir(&home);
    let (client_key, client_line) = client_key(&dir)?;
    let named = named_keys(&args.ssh_keys)?;
    let (keys, count) = authorized_keys(&home.join(".ssh"), &client_line, &named)?;
    let host_key = host_key(&dir)?;
    carried.push(File {
        path: KEYS_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(keys),
    });
    carried.push(File {
        path: HOST_KEY_PATH.to_owned(),
        mode: 0o600,
        content: Content::Bytes(host_key),
    });
    let mut config = config;
    if !config.ends_with('\n') {
        config.push('\n');
    }
    config.push_str("# Appended by `cargo xtask run-compositor --ssh`:\n");
    config.push_str(&format!(
        "exec-once = /{SERVER_PATH} -b 0.0.0.0 -p {GUEST_PORT} -h /{HOST_KEY_PATH} \
         --authorized-keys /{KEYS_PATH}\n"
    ));
    println!(
        "  ssh: sshdt on the guest's port {GUEST_PORT}, reached at 127.0.0.1:{port}; \
         {count} keys may log in"
    );
    // The line to paste, with this machine's guest key named outright. A
    // client whose own `~/.ssh` is empty -- another user here, a sandbox, a
    // CI step -- has no key to offer and is refused with
    // `Permission denied (publickey)`, which reads like a server refusing the
    // session rather than one that never saw a key.
    println!(
        "    ssh -i {} -p {port} root@127.0.0.1",
        client_key.display()
    );
    Ok(config)
}

/// Where this machine keeps the guest's keys: the host key the guest proves
/// itself with, and the key a client may log in to it with.
fn guest_dir(home: &Path) -> PathBuf {
    [".local", "share", "ferrix", "ssh"]
        .iter()
        .fold(home.to_path_buf(), |dir, name| dir.join(name))
}

/// Every public key allowed to log in, once each, as one `authorized_keys`
/// file, and how many there are: this machine's guest key first, then the
/// keys in `dir`'s `authorized_keys` and the `*.pub` files beside it, then
/// the ones `--ssh-key` named.
///
/// A line is a key when it is not blank and not a comment; one with options
/// in front of it is kept whole, as `sshd` would read it.
fn authorized_keys(dir: &Path, client: &str, named: &[String]) -> Result<(Vec<u8>, usize)> {
    let mut sources = vec![dir.join("authorized_keys")];
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut public: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "pub"))
            .collect();
        public.sort();
        sources.extend(public);
    }
    let mut texts = vec![client.to_owned()];
    for source in sources {
        match std::fs::read_to_string(&source) {
            Ok(text) => texts.push(text),
            // A source that is not there authorizes nobody and is no error:
            // most machines have no `authorized_keys` of their own.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::new(format!("reading {}: {error}", source.display())));
            }
        }
    }
    texts.extend(named.iter().cloned());
    let mut lines: Vec<String> = Vec::new();
    for text in texts {
        for line in text.lines().map(str::trim) {
            if !line.is_empty() && !line.starts_with('#') && !lines.iter().any(|kept| kept == line)
            {
                lines.push(line.to_owned());
            }
        }
    }
    let count = lines.len();
    let mut file = lines.join("\n");
    file.push('\n');
    Ok((file.into_bytes(), count))
}

/// What each `--ssh-key` names: the contents of a file, or the key itself
/// when it was written out on the command line.
///
/// A file is read whole, so `--ssh-key <somebody>/authorized_keys` carries
/// every key in it. Anything else is taken for a key, and a word that is
/// neither -- a path misspelt -- is refused rather than carried into a file
/// where `sshdt` would ignore it and nobody would know why they cannot log
/// in.
///
/// # Errors
///
/// A file that cannot be read, or a word that is neither a file nor a key.
fn named_keys(named: &[String]) -> Result<Vec<String>> {
    let mut texts = Vec::new();
    for key in named {
        let path = Path::new(key);
        if path.is_file() {
            texts.push(
                std::fs::read_to_string(path)
                    .map_err(|error| Error::new(format!("--ssh-key {key}: {error}")))?,
            );
        } else if key.starts_with("ssh-") || key.starts_with("ecdsa-") || key.starts_with("sk-") {
            texts.push(key.clone());
        } else {
            return Err(Error::new(format!(
                "--ssh-key {key}: not a file, and not a public key either"
            )));
        }
    }
    Ok(texts)
}

/// What `cargo xtask test-init` carries to run `sshdt` under socket
/// activation (`docs/INIT.md` §5.3, L9's gate): the server, the guest's host
/// key, and this machine's client key authorized, at the paths `--ssh` uses.
/// Answers the files, and the `ExecStart=` line that names them.
///
/// # Errors
///
/// No `sshdt` built for `arch` (`cargo xtask build-apps --app sshdt`), or a key
/// `ssh-keygen` could not make.
pub(crate) fn test_server(arch: crate::paths::Arch) -> Result<(Vec<File>, String)> {
    let mut files = crate::apps::taken(arch, &["sshdt"])?;
    if !files.iter().any(|file| file.path == SERVER_PATH) {
        return Err(Error::new(format!(
            "no sshdt built for {arch}: `cargo xtask build-apps --app sshdt` builds it"
        )));
    }
    let home = std::env::home_dir()
        .ok_or_else(|| Error::new("no home directory to find the guest's keys in"))?;
    let dir = guest_dir(&home);
    let (_, client_line) = client_key(&dir)?;
    files.push(File {
        path: KEYS_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(client_line.into_bytes()),
    });
    files.push(File {
        path: HOST_KEY_PATH.to_owned(),
        mode: 0o600,
        content: Content::Bytes(host_key(&dir)?),
    });
    let exec = format!("/{SERVER_PATH} -h /{HOST_KEY_PATH} --authorized-keys /{KEYS_PATH}");
    Ok((files, exec))
}

/// The private key a client on this machine logs in to a test boot's
/// guest with (`test_server`'s), made the first time.
///
/// # Errors
///
/// No home directory, or a key `ssh-keygen` could not make.
pub(crate) fn test_client_key() -> Result<PathBuf> {
    let home = std::env::home_dir()
        .ok_or_else(|| Error::new("no home directory to find the guest's keys in"))?;
    client_key(&guest_dir(&home)).map(|(path, _)| path)
}

/// The guest's host key, made with `ssh-keygen` the first time.
fn host_key(dir: &Path) -> Result<Vec<u8>> {
    let path = dir.join("ssh_host_ed25519_key");
    keygen(&path, "ferrix-guest", "the guest's host key")?;
    std::fs::read(&path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// The key a client on this machine may log in with, made with `ssh-keygen`
/// the first time: the private half's path, to name in an `ssh -i`, and the
/// public half's line, to authorize in the guest.
///
/// It is kept with the host key rather than in `~/.ssh`, because it belongs
/// to the guest and not to this machine's own logins: nothing but a Ferrix
/// boot is ever reached with it.
fn client_key(dir: &Path) -> Result<(PathBuf, String)> {
    let path = dir.join("id_ed25519");
    keygen(&path, "ferrix-client", "a key to log in to the guest with")?;
    let public = path.with_extension("pub");
    let line = std::fs::read_to_string(&public)
        .map_err(|error| Error::new(format!("reading {}: {error}", public.display())))?;
    if line.trim().is_empty() {
        return Err(Error::new(format!("{} holds no key", public.display())));
    }
    Ok((path, line))
}

/// An ed25519 key pair at `path`, made with `ssh-keygen` unless it is
/// already there.
fn keygen(path: &Path, comment: &str, what: &str) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| Error::new(format!("--ssh: {} has no directory", path.display())))?;
    std::fs::create_dir_all(dir)
        .map_err(|error| Error::new(format!("creating {}: {error}", dir.display())))?;
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(path)
        .status()
        .map_err(|error| Error::new(format!("--ssh: `ssh-keygen` would not start: {error}")))?;
    if !status.success() {
        return Err(Error::new(format!(
            "--ssh: `ssh-keygen` could not make {} ({status})",
            path.display()
        )));
    }
    println!("  ssh: made {what}, {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own under the system's temporary one.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferrix-ssh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn keys_are_gathered_once_each_without_comments() {
        let dir = scratch("keys");
        std::fs::write(
            dir.join("authorized_keys"),
            "# a comment\nssh-rsa AAAA one@a\n\nssh-ed25519 BBBB two@b\n",
        )
        .unwrap();
        std::fs::write(dir.join("id_ed25519.pub"), "ssh-ed25519 BBBB two@b\n").unwrap();
        std::fs::write(dir.join("id_other.pub"), "ssh-ed25519 CCCC three@c\n").unwrap();
        std::fs::write(dir.join("id_ed25519"), "PRIVATE, never read\n").unwrap();
        let (file, count) = authorized_keys(&dir, "ssh-ed25519 GGGG ferrix-client\n", &[]).unwrap();
        assert_eq!(count, 4, "one line per key, the repeated one once");
        assert_eq!(
            String::from_utf8(file).unwrap(),
            "ssh-ed25519 GGGG ferrix-client\nssh-rsa AAAA one@a\nssh-ed25519 BBBB two@b\n\
             ssh-ed25519 CCCC three@c\n",
            "this machine's guest key first, then ~/.ssh's own"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_ssh_directory_still_authorizes_the_guest_key() {
        let dir = scratch("none");
        let (file, count) = authorized_keys(&dir, "ssh-ed25519 GGGG ferrix-client\n", &[]).unwrap();
        assert_eq!(count, 1, "an empty ~/.ssh is no longer nobody");
        assert_eq!(
            String::from_utf8(file).unwrap(),
            "ssh-ed25519 GGGG ferrix-client\n",
            "which is what a client with no key of its own logs in with"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_key_is_a_file_or_the_key_itself() {
        let dir = scratch("named");
        let file = dir.join("theirs.pub");
        std::fs::write(&file, "ssh-ed25519 DDDD four@d\n").unwrap();
        let named =
            named_keys(&[file.display().to_string(), "ssh-rsa EEEE five@e".to_owned()]).unwrap();
        assert_eq!(named, ["ssh-ed25519 DDDD four@d\n", "ssh-rsa EEEE five@e"]);
        let (carried, count) =
            authorized_keys(&dir, "ssh-ed25519 GGGG ferrix-client\n", &named).unwrap();
        assert_eq!(count, 3, "the guest key, and the two named");
        assert!(
            String::from_utf8(carried)
                .unwrap()
                .ends_with("ssh-ed25519 DDDD four@d\nssh-rsa EEEE five@e\n"),
            "a named key is authorized, from a file or from the command line"
        );
        let missing = named_keys(&["~/.ssh/id_typo.pub".to_owned()]);
        assert!(
            missing.is_err(),
            "a path that is not there is a mistake, not a key nothing would match"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_port_costs_the_ssh_and_not_the_boot() {
        let args_for = |port: u16| {
            Args::parse(
                [
                    "run-compositor",
                    "--ssh",
                    &port.to_string(),
                    "--forward",
                    "8080:80",
                ]
                .into_iter()
                .map(str::to_owned),
            )
            .unwrap()
        };
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        let kept = checked(&args_for(port));
        assert_eq!(kept.ssh, None, "no SSH on a port somebody else holds");
        assert_eq!(
            kept.forwards,
            [crate::gateway::Forward {
                host: 8080,
                guest: 80
            }],
            "and no forward for it, while the others stay"
        );
        drop(holder);

        // A port let go of is free for anyone: the tests beside this one bind
        // ports of their own, and so do other runs on the same machine, and
        // one of them can take it before `checked` looks -- when "taken" is
        // the right answer. So a port freed a moment ago is asked about until
        // one is kept. A `checked` that never keeps a free port still fails
        // every time.
        let kept = (0..8).any(|_| {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            checked(&args_for(port)).ssh == Some(port)
        });
        assert!(kept, "a free port is kept");
    }

    #[test]
    fn keys_named_on_the_command_line_are_kept_in_order() {
        let args = Args::parse(
            [
                "run-compositor",
                "--ssh",
                "2222",
                "--ssh-key",
                "ssh-ed25519 AAAA one@a",
                "--ssh-key",
                "/home/somebody/.ssh/authorized_keys",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            args.ssh_keys,
            [
                "ssh-ed25519 AAAA one@a",
                "/home/somebody/.ssh/authorized_keys"
            ],
            "--ssh-key is repeatable"
        );
    }

    #[test]
    fn a_key_pair_is_made_once_and_read_back() {
        let dir = scratch("keygen").join("ssh");
        let (path, line) = match client_key(&dir) {
            Ok(made) => made,
            // A machine without `ssh-keygen` cannot be asked to make a key;
            // the rest of the test is about the one it would have made.
            Err(_) if Command::new("ssh-keygen").arg("-?").status().is_err() => return,
            Err(error) => panic!("{error}"),
        };
        assert!(
            path.is_file(),
            "the private half is where `ssh -i` names it"
        );
        assert!(
            line.starts_with("ssh-ed25519 "),
            "and the public half is a key: {line}"
        );
        let again = client_key(&dir).unwrap();
        assert_eq!(
            (again.0, again.1),
            (path, line),
            "a second boot authorizes the same key, or every boot would need a new -i"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn without_the_server_nothing_is_started() {
        let args = Args {
            ssh: Some(2222),
            ..Args::default()
        };
        let mut carried = Vec::new();
        let config =
            with_server("exec-once = /bin/term\n".to_owned(), &args, &mut carried).unwrap();
        assert_eq!(
            config, "exec-once = /bin/term\n",
            "no sshdt, no line for it"
        );
        assert!(carried.is_empty(), "and no keys carried for it");
    }
}
