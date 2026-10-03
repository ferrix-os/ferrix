//! `su`'s command line, as busybox's and util-linux's read it.

/// What was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    /// Whom to become: root unless named.
    pub(crate) user: String,
    /// `-`, `-l`, `--login`: a login shell in the target's home.
    pub(crate) login: bool,
    /// `-s SHELL`: root's choice of shell.
    pub(crate) shell: Option<String>,
    /// `-c COMMAND`.
    pub(crate) command: Option<String>,
}

/// Read `arguments`; `None` for one `su` does not take.
pub(crate) fn parse(arguments: &[String]) -> Option<Asked> {
    let mut asked = Asked {
        user: "root".to_owned(),
        login: false,
        shell: None,
        command: None,
    };
    let mut named = false;
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-" | "-l" | "--login" => asked.login = true,
            "-s" | "--shell" => asked.shell = Some(words.next()?.clone()),
            "-c" | "--command" => asked.command = Some(words.next()?.clone()),
            flag if flag.starts_with('-') => return None,
            user if !named && ferrix_auth_account::is_a_name(user) => {
                asked.user = user.to_owned();
                named = true;
            }
            _ => return None,
        }
    }
    Some(asked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_forms_the_gates_and_people_use() {
        let asked = parse(&words("")).unwrap();
        assert_eq!((asked.user.as_str(), asked.login), ("root", false));
        let asked = parse(&words("- ferrix")).unwrap();
        assert_eq!((asked.user.as_str(), asked.login), ("ferrix", true));
        let asked = parse(&["ferrix".into(), "-c".into(), "svc stop x".into()]).unwrap();
        assert_eq!(asked.command.as_deref(), Some("svc stop x"));
        let asked = parse(&words("-s /bin/sh ferrix -c id")).unwrap();
        assert_eq!(asked.shell.as_deref(), Some("/bin/sh"));
        assert_eq!(asked.command.as_deref(), Some("id"));
        assert!(parse(&words("-l")).is_some_and(|a| a.login && a.user == "root"));
    }

    #[test]
    fn what_su_does_not_take() {
        for bad in ["-x", "-c", "-s", "a b", "ferrix other", "-m"] {
            assert_eq!(parse(&words(bad)), None, "{bad:?}");
        }
    }
}
