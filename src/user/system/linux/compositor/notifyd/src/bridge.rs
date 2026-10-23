//! The bridge's reading half: bytes from `\0ferrix.devmgr.events` into
//! whole lines, and the replay a reconnect brings told from new events
//! (`docs/DESKTOP.md` §4.2). No socket here.

use std::collections::VecDeque;

/// The abstract socket name, without its leading NUL.
pub const SOCKET: &[u8] = b"ferrix.devmgr.events";

/// How many lines the helper replays to a reader that connects: its
/// `KEPT`. A reader remembers as many.
pub const KEPT: usize = 32;

/// Lines being read, and the ones already turned into notifications.
#[derive(Debug, Default)]
pub struct Lines {
    partial: Vec<u8>,
    seen: VecDeque<String>,
    /// Lines of a replay still to be matched against `seen`: set on a
    /// reconnect, cleared by the first line that is new.
    replaying: Option<VecDeque<String>>,
}

impl Lines {
    /// A new connection after an earlier one: the lines it replays first
    /// that were seen before are not news.
    pub fn reconnected(&mut self) {
        self.partial.clear();
        self.replaying = Some(self.seen.clone());
    }

    /// Take `bytes`, giving the whole lines in them that are news.
    pub fn take(&mut self, bytes: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(end) = self.partial.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            let line = String::from_utf8_lossy(line.get(..end).unwrap_or_default()).into_owned();
            if self.is_replayed(&line) {
                continue;
            }
            self.seen.push_back(line.clone());
            while self.seen.len() > KEPT {
                let _ = self.seen.pop_front();
            }
            out.push(line);
        }
        if self.partial.len() > 4096 {
            self.partial.clear();
        }
        out
    }

    /// Whether `line` is the next of a replay: the replay is the tail of
    /// what was seen, so it is matched from wherever its first line falls.
    fn is_replayed(&mut self, line: &str) -> bool {
        let Some(earlier) = &mut self.replaying else {
            return false;
        };
        while let Some(front) = earlier.pop_front() {
            if front == line {
                return true;
            }
        }
        self.replaying = None;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::Lines;

    /// Lines split across reads come out whole.
    #[test]
    fn lines_come_out_whole() {
        let mut lines = Lines::default();
        assert!(lines.take(b"died gpu 0x10 13").is_empty());
        assert_eq!(
            lines.take(b"7\nrestarted gpu 0x10 1\n"),
            ["died gpu 0x10 137", "restarted gpu 0x10 1"]
        );
    }

    /// A reconnect's replay of what was seen is dropped, and what follows
    /// it is news.
    #[test]
    fn a_replay_is_not_news() {
        let mut lines = Lines::default();
        let _ = lines.take(b"died snd 0x20 9\nrestarted snd 0x20 1\n");
        lines.reconnected();
        assert_eq!(
            lines.take(b"died snd 0x20 9\nrestarted snd 0x20 1\ndied net 0x30 9\n"),
            ["died net 0x30 9"]
        );
    }
}
