//! The notifications themselves: what is shown, what is kept, and how a
//! driver's death becomes one (`docs/DESKTOP.md` §4.2). Pure: no socket,
//! no surface, no clock but the one passed in.

/// How urgent a notification is: the `urgency` hint's byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// 0.
    Low,
    /// 1, and what a notification without the hint is.
    Normal,
    /// 2: stays until dismissed.
    Critical,
}

impl Urgency {
    /// From the hint's byte.
    #[must_use]
    pub const fn of(byte: u8) -> Self {
        match byte {
            0 => Self::Low,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }
}

/// How long a notification shows when its sender leaves it to the server.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Why a notification closed: `NotificationClosed`'s reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// 1: it timed out.
    Expired,
    /// 2: the person dismissed it.
    Dismissed,
    /// 3: `CloseNotification`.
    Closed,
}

impl Reason {
    /// The reason's number on the bus.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Expired => 1,
            Self::Dismissed => 2,
            Self::Closed => 3,
        }
    }
}

/// One notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// Its id, which the bus names it by.
    pub id: u32,
    /// The application that sent it.
    pub app: String,
    /// Its summary, shown bold.
    pub summary: String,
    /// Its body.
    pub body: String,
    /// Its actions: a key, which `ActionInvoked` carries, and a label;
    /// `default` is a click on the banner itself.
    pub actions: Vec<(String, String)>,
    /// How urgent.
    pub urgency: Urgency,
    /// When it goes, in the store's milliseconds; `None` for never.
    pub until_ms: Option<u64>,
    /// What notifications of the same thing share, so that a new one
    /// replaces the old rather than stacking: a device's, for a driver.
    pub key: Option<String>,
    /// How many times it has been sent under its key.
    pub count: u32,
    /// Whether it came from the bus (and so is owed `NotificationClosed`).
    pub on_bus: bool,
}

/// What a sender asks for: `Notify`'s arguments.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Asked {
    /// `app_name`.
    pub app: String,
    /// `replaces_id`, 0 for none.
    pub replaces: u32,
    /// `summary`.
    pub summary: String,
    /// `body`.
    pub body: String,
    /// `actions`, as the flat list of key and label the bus carries.
    pub actions: Vec<String>,
    /// The `urgency` hint.
    pub urgency: Option<u8>,
    /// `expire_timeout`: -1 for the server's, 0 for never.
    pub timeout_ms: i32,
}

/// Every notification shown and the recent ones kept for the list.
#[derive(Clone, Debug, Default)]
pub struct Store {
    shown: Vec<Note>,
    history: Vec<Note>,
    next_id: u32,
}

/// How many notifications the list keeps.
pub const HISTORY: usize = 20;

/// How many banners show at once; the oldest goes first past this.
pub const SHOWN: usize = 3;

impl Store {
    /// The banners on screen, oldest first.
    #[must_use]
    pub fn shown(&self) -> &[Note] {
        &self.shown
    }

    /// The recent notifications, newest first.
    #[must_use]
    pub fn history(&self) -> &[Note] {
        &self.history
    }

    /// A notification from the bus, at `now_ms`: its id, which is
    /// `replaces` where that is one still shown. Gives the id and the ids
    /// it pushed off the screen.
    pub fn notify(&mut self, asked: Asked, now_ms: u64) -> (u32, Vec<u32>) {
        let urgency = asked.urgency.map_or(Urgency::Normal, Urgency::of);
        let until_ms = match (urgency, asked.timeout_ms) {
            (Urgency::Critical, _) | (_, 0) => None,
            (_, timeout) if timeout > 0 => Some(now_ms + u64::from(timeout.cast_unsigned())),
            _ => Some(now_ms + DEFAULT_TIMEOUT_MS),
        };
        let actions = asked
            .actions
            .chunks(2)
            .filter_map(|pair| match pair {
                [key, label] => Some((key.clone(), label.clone())),
                _ => None,
            })
            .collect();
        let replaces = (asked.replaces != 0
            && self.shown.iter().any(|note| note.id == asked.replaces))
        .then_some(asked.replaces);
        let id = replaces.unwrap_or_else(|| self.fresh_id());
        self.put(Note {
            id,
            app: asked.app,
            summary: asked.summary,
            body: asked.body,
            actions,
            urgency,
            until_ms,
            key: None,
            count: 1,
            on_bus: true,
        })
    }

    /// A notification of the desktop's own under `key`: one already shown
    /// under it is replaced, its count carried on. Gives the id and the
    /// ids pushed off the screen.
    pub fn notify_keyed(
        &mut self,
        key: &str,
        summary: String,
        body: String,
        urgency: Urgency,
        now_ms: u64,
    ) -> (u32, Vec<u32>) {
        let held = self
            .shown
            .iter()
            .find(|note| note.key.as_deref() == Some(key))
            .map(|note| (note.id, note.count));
        let (id, count) = match held {
            Some((id, count)) => (id, count + 1),
            None => (self.fresh_id(), 1),
        };
        self.put(Note {
            id,
            app: "Ferrix".to_owned(),
            summary,
            body,
            actions: Vec::new(),
            urgency,
            until_ms: (urgency != Urgency::Critical).then_some(now_ms + DEFAULT_TIMEOUT_MS * 2),
            key: Some(key.to_owned()),
            count,
            on_bus: false,
        })
    }

    fn fresh_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }

    fn put(&mut self, note: Note) -> (u32, Vec<u32>) {
        let id = note.id;
        self.history.retain(|kept| kept.id != id);
        self.history.insert(0, note.clone());
        self.history.truncate(HISTORY);
        match self.shown.iter_mut().find(|shown| shown.id == id) {
            Some(shown) => *shown = note,
            None => self.shown.push(note),
        }
        let mut pushed = Vec::new();
        while self.shown.len() > SHOWN {
            pushed.push(self.shown.remove(0).id);
        }
        (id, pushed)
    }

    /// Take a notification off the screen. Gives it, if it was shown.
    pub fn close(&mut self, id: u32) -> Option<Note> {
        let at = self.shown.iter().position(|note| note.id == id)?;
        Some(self.shown.remove(at))
    }

    /// The notifications whose time is up at `now_ms`, taken off.
    pub fn expire(&mut self, now_ms: u64) -> Vec<Note> {
        let (gone, kept): (Vec<Note>, Vec<Note>) = std::mem::take(&mut self.shown)
            .into_iter()
            .partition(|note| note.until_ms.is_some_and(|until| until <= now_ms));
        self.shown = kept;
        gone
    }

    /// When the next one goes, if one will.
    #[must_use]
    pub fn next_expiry(&self) -> Option<u64> {
        self.shown.iter().filter_map(|note| note.until_ms).min()
    }

    /// Forget the list.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

/// What a driver is, to a person: its name in `devmgr`'s table, as the
/// thing it drives.
#[must_use]
pub fn driver_kind(driver: &str) -> &'static str {
    match driver {
        "gpu" | "intel" | "intel-keeper" | "nvrm" | "efifb" | "ltdc" | "gc400" => "Graphics",
        "net" | "yt6801" => "Network",
        "snd" => "Sound",
        "input" | "usbhid" => "Input",
        "blk" | "nvme" | "ahci" | "sdmmc" => "Disk",
        "usbdev" => "USB",
        "power" => "Power",
        _ => "A",
    }
}

/// A line of `devmgr-events` as a notification: its key (the device),
/// summary, body and urgency. `None` for a line that is not one.
#[must_use]
pub fn from_driver_line(line: &str) -> Option<(String, String, String, Urgency)> {
    let mut words = line.split_whitespace();
    let (what, driver, location, value) =
        (words.next()?, words.next()?, words.next()?, words.next()?);
    let value: u32 = value.parse().ok()?;
    let kind = driver_kind(driver);
    let key = format!("driver:{location}");
    let (summary, body, urgency) = match what {
        "died" => (
            format!("{kind} driver crashed, restarting"),
            format!("{driver} ended with status {value}"),
            Urgency::Normal,
        ),
        "restarted" => (
            format!("{kind} driver restarted"),
            format!(
                "{driver} is running again ({value} restart{})",
                if value == 1 { "" } else { "s" }
            ),
            Urgency::Normal,
        ),
        "gave-up" => (
            format!("{kind} driver stopped after {value} restarts"),
            format!("{driver} is not started again; its device stays off until the next boot"),
            Urgency::Critical,
        ),
        _ => return None,
    };
    Some((key, summary, body, urgency))
}

#[cfg(test)]
mod tests {
    use super::{Asked, Reason, Store, Urgency, from_driver_line};

    fn asked(summary: &str) -> Asked {
        Asked {
            app: "Files".to_owned(),
            summary: summary.to_owned(),
            timeout_ms: -1,
            ..Asked::default()
        }
    }

    /// A notification shows until its time, a critical one until closed,
    /// and `replaces_id` replaces in place.
    #[test]
    fn notifications_show_expire_and_replace() {
        let mut store = Store::default();
        let (first, _) = store.notify(asked("one"), 0);
        let mut critical = asked("two");
        critical.urgency = Some(2);
        let (second, _) = store.notify(critical, 0);
        assert_eq!(store.shown().len(), 2);
        assert_eq!(store.next_expiry(), Some(5_000));
        let mut again = asked("one again");
        again.replaces = first;
        let (same, _) = store.notify(again, 100);
        assert_eq!(same, first);
        assert_eq!(store.shown().len(), 2);
        let gone = store.expire(5_100);
        assert_eq!(gone.iter().map(|note| note.id).collect::<Vec<_>>(), [first]);
        assert!(store.close(second).is_some());
        assert!(store.shown().is_empty());
        assert_eq!(store.history().len(), 2, "kept for the list");
        assert_eq!(Reason::Dismissed.code(), 2);
    }

    /// Past three on screen the oldest goes.
    #[test]
    fn the_oldest_banner_makes_room() {
        let mut store = Store::default();
        for summary in ["a", "b", "c"] {
            let _ = store.notify(asked(summary), 0);
        }
        let (_, pushed) = store.notify(asked("d"), 0);
        assert_eq!(pushed, [1]);
        assert_eq!(store.shown().len(), 3);
    }

    /// A driver's lines become one notification per device, counted, and a
    /// gave-up is critical.
    #[test]
    fn a_drivers_crashes_collapse_into_one() {
        let mut store = Store::default();
        let mut id = 0;
        for line in [
            "died gpu 0x00000010 137",
            "restarted gpu 0x00000010 1",
            "died gpu 0x00000010 137",
        ] {
            let (key, summary, body, urgency) = from_driver_line(line).expect("a driver line");
            assert_eq!(key, "driver:0x00000010");
            let (got, _) = store.notify_keyed(&key, summary, body, urgency, 0);
            if id != 0 {
                assert_eq!(got, id, "the same banner");
            }
            id = got;
        }
        assert_eq!(store.shown().len(), 1);
        assert_eq!(store.shown()[0].count, 3);
        assert_eq!(
            store.shown()[0].summary,
            "Graphics driver crashed, restarting"
        );
        let (_, summary, _, urgency) =
            from_driver_line("gave-up snd 0x00000020 8").expect("a line");
        assert_eq!(summary, "Sound driver stopped after 8 restarts");
        assert_eq!(urgency, Urgency::Critical);
        assert!(from_driver_line("nonsense").is_none());
    }
}
