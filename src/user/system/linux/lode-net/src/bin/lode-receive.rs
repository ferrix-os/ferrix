//! `lode-receive`: Ferrix machines' logs, kept on a Linux host
//! (`docs/LOGGING.md` §10.3). An ordinary user's program.
//!
//! ```text
//! lode-receive pair --machine NAME --server-name HOST [--server-name HOST]... [--port P] [--root DIR] [--json]
//! lode-receive run [--listen ADDR:PORT] [--root DIR] [--allow-unpaired]
//! lode-receive check [--root DIR] [--json]
//! lode-receive install-service [--listen ADDR:PORT] [--root DIR] [--json]
//! lode-receive show --machine DIR [--root DIR]     a machine's records, journald's JSON a line
//! ```
//!
//! Exit status: 0 done; 1 an error running; 2 usage; 3 `check` found no
//! receiver running; 4 certificate or key trouble.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use lode_net::pki::Pki;
use lode_net::receive::{self, Receiver};

/// What went wrong, and the status it ends with.
struct Failure(u8, String);

/// The options every subcommand reads.
#[derive(Default)]
struct Options {
    /// `--root`.
    root: Option<PathBuf>,
    /// `--json`.
    json: bool,
    /// `--machine`.
    machine: Option<String>,
    /// `--server-name`, each.
    servers: Vec<String>,
    /// `--port`.
    port: Option<u16>,
    /// `--listen`.
    listen: Option<String>,
    /// `--allow-unpaired`.
    unpaired: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|arg| arg == "--json");
    let result = match args.split_first() {
        Some((command, rest)) => parse(rest).and_then(|options| match command.as_str() {
            "pair" => pair(&options),
            "run" => run(&options),
            "check" => check(&options),
            "install-service" => install(&options),
            "show" => show(&options),
            other => Err(Failure(
                2,
                format!("{other}: pair, run, check or install-service"),
            )),
        }),
        None => Err(Failure(
            2,
            String::from("pair, run, check or install-service"),
        )),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(Failure(code, why)) => {
            if json {
                println!("{{\"error\":{}}}", quoted(&why));
            } else {
                eprintln!("lode-receive: {why}");
            }
            ExitCode::from(code)
        }
    }
}

/// Read the options after the subcommand.
fn parse(args: &[String]) -> Result<Options, Failure> {
    let mut options = Options::default();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let mut value = |name: &str| {
            words
                .next()
                .cloned()
                .ok_or_else(|| Failure(2, format!("{name} needs a value")))
        };
        match word.as_str() {
            "--root" => options.root = Some(PathBuf::from(value("--root")?)),
            "--json" => options.json = true,
            "--machine" => options.machine = Some(value("--machine")?),
            "--server-name" => options.servers.push(value("--server-name")?),
            "--port" => {
                let port = value("--port")?;
                options.port = Some(
                    port.parse()
                        .ok()
                        .filter(|&port| port != 0)
                        .ok_or_else(|| Failure(2, format!("{port}: not a port")))?,
                );
            }
            "--listen" => options.listen = Some(value("--listen")?),
            "--allow-unpaired" => options.unpaired = true,
            other => return Err(Failure(2, format!("{other}: not an option"))),
        }
    }
    Ok(options)
}

/// `--root`, or `~/.local/share/ferrix/lode-receive`.
fn root(options: &Options) -> Result<PathBuf, Failure> {
    if let Some(root) = &options.root {
        return Ok(root.clone());
    }
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))
        .ok_or_else(|| {
            Failure(
                1,
                String::from("no HOME to keep the stores under; give --root"),
            )
        })?;
    Ok(home.join("ferrix/lode-receive"))
}

/// `--listen`, or every address on `--port` (19532).
fn listen(options: &Options) -> String {
    options
        .listen
        .clone()
        .unwrap_or_else(|| format!("0.0.0.0:{}", options.port.unwrap_or(receive::PORT)))
}

/// `pair`.
fn pair(options: &Options) -> Result<u8, Failure> {
    let name = options
        .machine
        .as_deref()
        .ok_or_else(|| Failure(2, String::from("pair needs --machine NAME")))?;
    let root = root(options)?;
    let port = options.port.unwrap_or(receive::PORT);
    let paired = Pki::at(&root)
        .pair(name, &options.servers)
        .map_err(|why| Failure(4, why))?;
    let remote_dir = format!("/etc/ferrix/lode-upload.d/{name}");
    let server = options.servers.first().cloned().unwrap_or_default();
    let line = format!(
        "{name} https://{server}:{port}/upload export ca={remote_dir}/ca.pem \
         cert={remote_dir}/client.pem key={remote_dir}/client.key"
    );
    let files = [
        (&paired.ca, format!("{remote_dir}/ca.pem"), "0644"),
        (
            &paired.client_cert,
            format!("{remote_dir}/client.pem"),
            "0644",
        ),
        (
            &paired.client_key,
            format!("{remote_dir}/client.key"),
            "0600",
        ),
    ];
    if options.json {
        let files: Vec<String> = files
            .iter()
            .map(|(local, remote, mode)| {
                format!(
                    "{{\"local\":{},\"remote\":{},\"mode\":{}}}",
                    quoted(&local.display().to_string()),
                    quoted(remote),
                    quoted(mode)
                )
            })
            .collect();
        println!(
            "{{\"machine\":{},\"listen\":{},\"ca_fingerprint\":{},\"server_fingerprint\":{},\
             \"remote_files\":[{}],\"remote_config_file\":\"/etc/ferrix/lode-upload.conf\",\
             \"remote_config_line\":{},\"remote_restart\":\"svc enable lode-upload.service && svc restart lode-upload.service\"}}",
            quoted(name),
            quoted(&format!("0.0.0.0:{port}")),
            quoted(&paired.ca_fingerprint),
            quoted(&paired.server_fingerprint),
            files.join(","),
            quoted(&line)
        );
    } else {
        println!(
            "paired {name}; server certificate {}",
            paired.server_fingerprint
        );
        println!("copy to the machine, as root:");
        for (local, remote, mode) in &files {
            println!("  {} -> {remote} (mode {mode})", local.display());
        }
        println!("and put this line in its /etc/ferrix/lode-upload.conf:");
        println!("  {line}");
        println!("then: svc enable lode-upload.service && svc restart lode-upload.service");
    }
    Ok(0)
}

/// `run`.
fn run(options: &Options) -> Result<u8, Failure> {
    let root = root(options)?;
    let listen = listen(options);
    let pki = Pki::at(&root);
    let (cert, key) = pki.server();
    let tls = lode_net::tls::server(&cert, &key, &pki.ca(), options.unpaired)
        .map_err(|why| Failure(4, format!("{why} (run `lode-receive pair` first)")))?;
    let Some(_lock) =
        receive::lock(&root, &listen).map_err(|error| Failure(1, error.to_string()))?
    else {
        return Err(Failure(
            1,
            format!("another lode-receive keeps {}", root.display()),
        ));
    };
    let listener = std::net::TcpListener::bind(&listen)
        .map_err(|error| Failure(1, format!("{listen}: {error}")))?;
    println!(
        "lode-receive: on {listen}, keeping {}{}",
        root.display(),
        if options.unpaired {
            ", unpaired machines taken"
        } else {
            ""
        }
    );
    receive::serve(&Arc::new(Receiver::new(&root)), &listener, &tls);
    Ok(1)
}

/// `check`.
fn check(options: &Options) -> Result<u8, Failure> {
    let root = root(options)?;
    let held = receive::holder(&root);
    let (cert, _) = Pki::at(&root).server();
    let fingerprint = lode_net::tls::certificates(&cert)
        .ok()
        .and_then(|certs| certs.first().map(|cert| lode_net::tls::fingerprint(cert)));
    let mut machines = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let Ok(dir) = entry.file_name().into_string() else {
                continue;
            };
            if dir.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            let label = std::fs::read_to_string(entry.path().join("machine")).unwrap_or_default();
            let field = |name: &str| {
                label
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .map(str::to_owned)
            };
            let records: usize = lode_net::store::segments(&[entry.path()])
                .iter()
                .map(|segment| {
                    segment.summary().map_or_else(
                        || segment.records().len(),
                        |summary| usize::try_from(summary.records).unwrap_or(0),
                    )
                })
                .sum();
            machines.push((dir, field("hostname="), field("last_seen="), records));
        }
    }
    machines.sort();
    if options.json {
        let list: Vec<String> = machines
            .iter()
            .map(|(dir, host, seen, records)| {
                format!(
                    "{{\"dir\":{},\"hostname\":{},\"last_seen\":{},\"records\":{records}}}",
                    quoted(dir),
                    host.as_deref().map_or_else(|| String::from("null"), quoted),
                    seen.as_deref().map_or_else(|| String::from("null"), quoted)
                )
            })
            .collect();
        println!(
            "{{\"running\":{},\"pid\":{},\"listen\":{},\"server_fingerprint\":{},\"machines\":[{}]}}",
            held.is_some(),
            held.as_ref()
                .map_or_else(|| String::from("null"), |(pid, _)| pid.to_string()),
            held.as_ref()
                .map_or_else(|| String::from("null"), |(_, listen)| quoted(listen)),
            fingerprint
                .as_deref()
                .map_or_else(|| String::from("null"), quoted),
            list.join(",")
        );
    } else {
        match &held {
            Some((pid, listen)) => println!("running: pid {pid} on {listen}"),
            None => println!("not running"),
        }
        if let Some(fingerprint) = &fingerprint {
            println!("server certificate: {fingerprint}");
        }
        for (dir, host, seen, records) in &machines {
            println!(
                "{dir}  {}  {records} record(s), last seen {}",
                host.as_deref().unwrap_or("?"),
                seen.as_deref().unwrap_or("never")
            );
        }
    }
    Ok(if held.is_some() { 0 } else { 3 })
}

/// `install-service`: a systemd user unit, enabled and started.
fn install(options: &Options) -> Result<u8, Failure> {
    let root = root(options)?;
    let listen = listen(options);
    let program = std::env::current_exe().map_err(|error| Failure(1, error.to_string()))?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .ok_or_else(|| Failure(1, String::from("no HOME for the user unit")))?;
    let unit = config.join("systemd/user/lode-receive.service");
    let text = format!(
        "# Written by `lode-receive install-service` (docs/LOGGING.md §10.3).\n\
         [Unit]\nDescription=lode-receive: logs from Ferrix machines\n\n\
         [Service]\nExecStart={} run --root {} --listen {listen}\nRestart=on-failure\n\
         RestartSec=5s\n\n[Install]\nWantedBy=default.target\n",
        program.display(),
        root.display()
    );
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|error| Failure(1, error.to_string()))?;
    }
    lode_net::store::replace(&unit, text.as_bytes())
        .map_err(|error| Failure(1, format!("{}: {error}", unit.display())))?;
    let systemctl = |args: &[&str]| {
        std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()
            .is_ok_and(|status| status.success())
    };
    let enabled = systemctl(&["daemon-reload"]) && systemctl(&["enable", "lode-receive.service"]);
    let started = enabled && systemctl(&["restart", "lode-receive.service"]);
    if options.json {
        println!(
            "{{\"unit\":{},\"enabled\":{enabled},\"started\":{started}}}",
            quoted(&unit.display().to_string())
        );
    } else {
        println!("{}: enabled {enabled}, started {started}", unit.display());
    }
    Ok(if started { 0 } else { 1 })
}

/// `show`: a machine's records, oldest first, journald's JSON a line.
fn show(options: &Options) -> Result<u8, Failure> {
    let root = root(options)?;
    let dir = options
        .machine
        .as_deref()
        .filter(|dir| !dir.is_empty() && !dir.contains('/') && !dir.starts_with('.'))
        .ok_or_else(|| {
            Failure(
                2,
                String::from("show needs --machine DIR (a name `check` lists)"),
            )
        })?;
    let zone = ferrix_lode_core::time::Zone::utc();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for segment in lode_net::store::segments(&[root.join(dir)]) {
        let mut bytes = Vec::new();
        for record in segment.records() {
            ferrix_lode_core::format::write(
                &record,
                ferrix_lode_core::format::Output::Json,
                &zone,
                &mut bytes,
            );
        }
        std::io::Write::write_all(&mut out, &bytes)
            .map_err(|error| Failure(1, error.to_string()))?;
    }
    std::io::Write::flush(&mut out).map_err(|error| Failure(1, error.to_string()))?;
    Ok(0)
}

/// `text` as a JSON string.
fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
