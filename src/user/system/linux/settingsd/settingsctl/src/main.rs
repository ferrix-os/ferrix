//! `/bin/settingsctl`: `settingsd`'s requests from a command line
//! (`docs/SETTINGS-WRITER.md` §7).
//!
//! ```text
//! settingsctl [--dry-run] [--as-given] set-hostname NAME
//! settingsctl [--dry-run] [--as-given] service-enable-persistent UNIT on|off
//! ```
//!
//! It prints `settingsctl: done: TEXT`, `settingsctl: would: TEXT` or
//! `settingsctl: refused: KIND: TEXT`, and exits 0, 0 or 1 (2 for a command
//! line it does not take). A password `authd` asks for is read from the
//! terminal, or one line of standard input, as `authctl try` reads it.
//! `--as-given` sends the request without checking its rules here first,
//! so that `settingsd`'s own refusal is what answers: for gates.

use std::io::Write as _;
use std::process::ExitCode;

use ferrix_auth_client::Terminal;
use ferrix_settings_client::{Client, Conversation, Outcome, Refusal, Request, Step};

const USAGE: &str = "usage: settingsctl [--dry-run] [--as-given] set-hostname NAME\n       \
                     settingsctl [--dry-run] [--as-given] service-enable-persistent UNIT on|off";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut dry_run = false;
    let mut as_given = false;
    let mut rest = arguments.as_slice();
    while let Some((first, more)) = rest.split_first() {
        match first.as_str() {
            "--dry-run" => dry_run = true,
            "--as-given" => as_given = true,
            _ => break,
        }
        rest = more;
    }
    let Some(request) = parse(rest) else {
        say(USAGE);
        return ExitCode::from(2);
    };
    let client = Client::new();
    let result = match (dry_run, as_given) {
        (true, false) => client.dry_run(&request),
        (false, false) => client.apply(&request, &mut Terminal::new()),
        (dry_run, true) => client
            .begin_as_given(&request, dry_run)
            .and_then(|talk| finish(talk, &mut Terminal::new())),
    };
    match result {
        Ok(outcome) => {
            let word = if dry_run { "would" } else { "done" };
            let text = outcome.text.strip_prefix("would ").unwrap_or(&outcome.text);
            say(&format!("settingsctl: {word}: {text}"));
            ExitCode::SUCCESS
        }
        Err(refusal) => {
            say(&format!(
                "settingsctl: refused: {}: {}",
                refusal.kind.name(),
                refusal.text
            ));
            ExitCode::FAILURE
        }
    }
}

/// The request a command line names.
fn parse(arguments: &[String]) -> Option<Request> {
    match arguments {
        [verb, name] if verb == "set-hostname" => Some(Request::SetHostname { name: name.clone() }),
        [verb, unit, state] if verb == "service-enable-persistent" => {
            let enabled = match state.as_str() {
                "on" => true,
                "off" => false,
                _ => return None,
            };
            Some(Request::ServiceEnablePersistent {
                unit: unit.clone(),
                enabled,
            })
        }
        _ => None,
    }
}

/// Carry a conversation to its end, asking `person`.
fn finish(
    mut talk: Conversation,
    person: &mut dyn ferrix_settings_client::Person,
) -> Result<Outcome, Refusal> {
    loop {
        match talk.step() {
            Step::Prompt { visible, text } => match person.ask(visible, &text) {
                Some(secret) => talk.answer(secret)?,
                None => {
                    talk.cancel();
                    return Err(Refusal::new(
                        ferrix_settings_client::RefusalKind::Cancelled,
                        "no answer was given",
                    ));
                }
            },
            Step::Info(text) => person.tell(&text, false),
            Step::Done(outcome) => return Ok(outcome),
            Step::Refused(refusal) => return Err(refusal),
        }
    }
}

/// Say a line on standard output.
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn each_verb_is_its_request_and_nothing_else_is_taken() {
        assert_eq!(
            parse(&words("set-hostname box")),
            Some(Request::SetHostname {
                name: "box".to_owned()
            })
        );
        assert_eq!(
            parse(&words("service-enable-persistent sshd.service off")),
            Some(Request::ServiceEnablePersistent {
                unit: "sshd.service".to_owned(),
                enabled: false
            })
        );
        for bad in [
            "",
            "set-hostname",
            "set-hostname a b",
            "service-enable-persistent sshd.service maybe",
            "write-file /etc/shadow x",
        ] {
            assert_eq!(parse(&words(bad)), None, "{bad}");
        }
    }
}
