//! What the probe keeps and reports, with no X in it: each event's arrival,
//! each frame's events and swap, and which completion first showed each
//! event; and the pairing of the probe's report with the injector's log.

/// Present's `Skip` mode: the pixmap was never shown.
const SKIPPED: u8 = 2;

/// One input event, and the completion that first showed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    arrival: u64,
    /// The msc of the last completion before it arrived.
    msc_before: Option<u64>,
    shown: bool,
}

/// A frame in flight: its serial, how many events it carries, and when it
/// was handed to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    serial: u32,
    carries: usize,
    swap: u64,
}

/// One event's line once shown, and the numbers the summary takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shown {
    pub(crate) number: usize,
    pub(crate) arrival: u64,
    pub(crate) swap: u64,
    pub(crate) ust: u64,
    pub(crate) msc: u64,
    pub(crate) frames: Option<u64>,
}

impl Shown {
    /// From the event's arrival to the start of the scan-out that showed
    /// it, in nanoseconds.
    pub(crate) fn latency(&self) -> i64 {
        to_signed(self.ust.saturating_mul(1000)) - to_signed(self.arrival)
    }

    /// The line the probe prints for it.
    pub(crate) fn line(&self) -> String {
        let frames = self.frames.map_or("-".to_owned(), |n| n.to_string());
        format!(
            "xlatency: shown {} arrival {} swap {} ust {} msc {} frames {} in {}",
            self.number,
            self.arrival,
            self.swap,
            self.ust,
            self.msc,
            frames,
            millis(self.latency())
        )
    }
}

fn to_signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Nanoseconds as milliseconds with three places.
pub(crate) fn millis(nanos: i64) -> String {
    let sign = if nanos < 0 { "-" } else { "" };
    let micros = nanos.unsigned_abs() / 1000;
    format!("{sign}{}.{:03}", micros / 1000, micros % 1000)
}

/// The probe's record.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    events: Vec<Event>,
    frames: Vec<Frame>,
    last: Option<(u64, u64)>,
    first: Option<(u64, u64)>,
    completions: usize,
    pub(crate) shown: Vec<Shown>,
}

impl Tracker {
    /// An event arrived at `now`.
    pub(crate) fn arrive(&mut self, now: u64) {
        self.events.push(Event {
            arrival: now,
            msc_before: self.last.map(|(_, msc)| msc),
            shown: false,
        });
    }

    /// How many events a frame drawn at `now` carries: those that arrived
    /// `delay` or more before.
    pub(crate) fn applied(&self, now: u64, delay: u64) -> usize {
        self.events
            .iter()
            .filter(|event| event.arrival.saturating_add(delay) <= now)
            .count()
    }

    /// How many events have arrived.
    pub(crate) fn arrived(&self) -> usize {
        self.events.len()
    }

    /// The frame `serial`, carrying `carries` events, was handed over at
    /// `now`.
    pub(crate) fn presented(&mut self, serial: u32, carries: usize, now: u64) {
        self.frames.push(Frame {
            serial,
            carries,
            swap: now,
        });
    }

    /// The msc of the last completion.
    pub(crate) fn last_msc(&self) -> Option<u64> {
        self.last.map(|(_, msc)| msc)
    }

    /// The refresh, in nanoseconds, from the first and last completions.
    pub(crate) fn refresh(&self) -> Option<u64> {
        let ((first_ust, first_msc), (last_ust, last_msc)) = (self.first?, self.last?);
        let frames = last_msc.checked_sub(first_msc).filter(|n| *n > 0)?;
        Some(last_ust.checked_sub(first_ust)?.saturating_mul(1000) / frames)
    }

    /// The frame `serial` completed in `mode` at `ust` and `msc`: the
    /// events it is the first to show.
    pub(crate) fn complete(&mut self, serial: u32, mode: u8, ust: u64, msc: u64) -> Vec<Shown> {
        let Some(at) = self.frames.iter().position(|frame| frame.serial == serial) else {
            return Vec::new();
        };
        let frame = self.frames.remove(at);
        if mode == SKIPPED {
            return Vec::new();
        }
        self.completions += 1;
        if self.first.is_none() {
            self.first = Some((ust, msc));
        }
        self.last = Some((ust, msc));
        let mut shown = Vec::new();
        for (index, event) in self.events.iter_mut().enumerate().take(frame.carries) {
            if event.shown {
                continue;
            }
            event.shown = true;
            shown.push(Shown {
                number: index + 1,
                arrival: event.arrival,
                swap: frame.swap,
                ust,
                msc,
                frames: event.msc_before.and_then(|before| msc.checked_sub(before)),
            });
        }
        self.shown.extend_from_slice(&shown);
        shown
    }
}

/// The value at percentile `p` of `sorted` by nearest rank.
pub(crate) fn percentile<T: Copy>(sorted: &[T], p: u64) -> Option<T> {
    let n = u64::try_from(sorted.len()).ok()?;
    let rank = (n * p).div_ceil(100).max(1);
    sorted.get(usize::try_from(rank - 1).ok()?).copied()
}

/// The median, the 99th percentile, the least and the most of `values`.
pub(crate) fn spread(values: &[i64]) -> Option<(i64, i64, i64, i64)> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Some((
        percentile(&sorted, 50)?,
        percentile(&sorted, 99)?,
        *sorted.first()?,
        *sorted.last()?,
    ))
}

/// The probe's own summary: from arrival to scan-out, and in frames.
pub(crate) fn summary(tracker: &Tracker) -> String {
    let latencies: Vec<i64> = tracker.shown.iter().map(Shown::latency).collect();
    let frames: Vec<i64> = tracker
        .shown
        .iter()
        .filter_map(|shown| shown.frames.map(to_signed))
        .collect();
    let refresh = tracker
        .refresh()
        .map_or("-".to_owned(), |nanos| millis(to_signed(nanos)));
    let first_and_last_msc = match (tracker.first, tracker.last) {
        (Some((_, first)), Some((_, last))) => format!("{first} to {last}"),
        _ => "-".to_owned(),
    };
    let mut line = format!(
        "xlatency: summary events {} shown {} frames {} msc {first_and_last_msc} refresh {refresh}",
        tracker.arrived(),
        tracker.shown.len(),
        tracker.completions
    );
    if let Some((median, p99, low, high)) = spread(&latencies) {
        line.push_str(&format!(
            " arrival-to-scanout median {} p99 {} min {} max {}",
            millis(median),
            millis(p99),
            millis(low),
            millis(high)
        ));
    }
    if let Some((median, p99, _, high)) = spread(&frames) {
        line.push_str(&format!(" frames median {median} p99 {p99} max {high}"));
    }
    line
}

/// The send times in the injector's log, sorted.
pub(crate) fn sends(log: &str) -> Vec<u64> {
    let mut times: Vec<u64> = log
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("vptr: send ")?;
            rest.split_whitespace().nth(1)?.parse().ok()
        })
        .collect();
    times.sort_unstable();
    times
}

/// The probe's `shown` lines, back into numbers.
pub(crate) fn shown_lines(report: &str) -> Vec<Shown> {
    report
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line
                .split("xlatency: shown ")
                .nth(1)?
                .split_whitespace()
                .collect();
            let field = |name: &str| -> Option<&str> {
                let at = fields.iter().position(|field| *field == name)?;
                fields.get(at + 1).copied()
            };
            Some(Shown {
                number: fields.first()?.parse().ok()?,
                arrival: field("arrival")?.parse().ok()?,
                swap: field("swap")?.parse().ok()?,
                ust: field("ust")?.parse().ok()?,
                msc: field("msc")?.parse().ok()?,
                frames: field("frames").and_then(|n| n.parse().ok()),
            })
        })
        .collect()
}

/// Which send each shown event came of. When as many events were shown as
/// motions sent, the n-th event is the n-th motion: "by order". Otherwise
/// a motion was lost or split, and each event is paired with the last
/// motion sent before it arrived, "by time", leaving out an event whose
/// motion an earlier event already took, which is counted. The time
/// pairing is wrong when the probe or the server was held up past the next
/// send, so a run that is not paired by order is not one to quote.
fn pairs(sends: &[u64], shown: &[Shown]) -> (Vec<(u64, Shown)>, &'static str, usize) {
    if sends.len() == shown.len() {
        let paired = sends.iter().copied().zip(shown.iter().copied()).collect();
        return (paired, "order", 0);
    }
    let mut paired = Vec::new();
    let mut used = Vec::new();
    let mut repeated = 0;
    for event in shown {
        let Some(index) = sends
            .partition_point(|&sent| sent <= event.arrival)
            .checked_sub(1)
        else {
            continue;
        };
        if used.contains(&index) {
            repeated += 1;
            continue;
        }
        used.push(index);
        if let Some(&sent) = sends.get(index) {
            paired.push((sent, *event));
        }
    }
    (paired, "time", repeated)
}

/// `xlatency summarize`: each shown event paired with the motion it came
/// of ([`pairs`]), and from that send to the scan-out.
pub(crate) fn summarize(log: &str, report: &str) -> String {
    let sends = sends(log);
    let shown = shown_lines(report);
    let (paired, by, repeated) = pairs(&sends, &shown);
    let latencies: Vec<i64> = paired
        .iter()
        .map(|(sent, event)| to_signed(event.ust.saturating_mul(1000)) - to_signed(*sent))
        .collect();
    let transit: Vec<i64> = paired
        .iter()
        .map(|(sent, event)| to_signed(event.arrival) - to_signed(*sent))
        .collect();
    let mut line = format!(
        "xlatency: input-to-scanout sent {} shown {} paired {} by {by} repeated {repeated}",
        sends.len(),
        shown.len(),
        latencies.len()
    );
    if let Some((median, p99, low, high)) = spread(&latencies) {
        line.push_str(&format!(
            " median {} p99 {} min {} max {}",
            millis(median),
            millis(p99),
            millis(low),
            millis(high)
        ));
    }
    if let Some((median, p99, _, _)) = spread(&transit) {
        line.push_str(&format!(
            " send-to-arrival median {} p99 {}",
            millis(median),
            millis(p99)
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_shown_by_the_first_completion_that_carries_it() {
        let mut tracker = Tracker::default();
        tracker.presented(1, 0, 1_000);
        assert!(tracker.complete(1, 0, 16_000, 100).is_empty());
        tracker.arrive(20_000_000);
        // The frame in flight was drawn before it, and shows nothing new.
        tracker.presented(2, 0, 19_000_000);
        assert!(tracker.complete(2, 0, 32_667, 101).is_empty());
        tracker.presented(3, tracker.applied(33_000_000, 0), 33_000_000);
        let shown = tracker.complete(3, 1, 49_334, 102);
        assert_eq!(shown.len(), 1);
        let first = shown.first().copied();
        assert_eq!(first.map(|s| (s.number, s.frames)), Some((1, Some(2))));
        assert_eq!(first.map(|s| s.latency()), Some(29_334_000));
        // Later frames carry it too, and do not show it again.
        tracker.presented(4, 1, 50_000_000);
        assert!(tracker.complete(4, 0, 66_000, 103).is_empty());
        assert_eq!(tracker.refresh(), Some(16_666_666));
    }

    #[test]
    fn a_delay_holds_an_event_out_of_the_next_frame_and_a_skip_shows_nothing() {
        let mut tracker = Tracker::default();
        tracker.arrive(10_000_000);
        assert_eq!(tracker.applied(20_000_000, 16_700_000), 0);
        assert_eq!(tracker.applied(26_700_000, 16_700_000), 1);
        tracker.presented(7, 1, 27_000_000);
        assert!(tracker.complete(7, SKIPPED, 40_000, 5).is_empty());
        assert_eq!(tracker.last_msc(), None);
    }

    #[test]
    fn the_report_reads_back_and_pairs_with_the_last_send_before_arrival() {
        let mut tracker = Tracker::default();
        tracker.presented(1, 0, 0);
        let _ = tracker.complete(1, 0, 1_000, 10);
        tracker.arrive(3_000_000);
        tracker.presented(2, 1, 4_000_000);
        let report: String = tracker
            .complete(2, 0, 21_000, 12)
            .iter()
            .map(|shown| shown.line() + "\n")
            .collect();
        assert_eq!(
            report,
            "xlatency: shown 1 arrival 3000000 swap 4000000 ust 21000 msc 12 frames 2 in 18.000\n"
        );
        assert_eq!(shown_lines(&report), tracker.shown);
        let log = "vptr: send 1 2500000\nvptr: send 2 900000000\n";
        assert_eq!(
            summarize(log, &report),
            "xlatency: input-to-scanout sent 2 shown 1 paired 1 by time repeated 0 \
             median 18.500 p99 18.500 min 18.500 max 18.500 \
             send-to-arrival median 0.500 p99 0.500"
        );
        assert!(summarize("vptr: send 1 2500000\n", &report).contains("paired 1 by order"));
    }

    /// Three events held up together behind the third send: by order each
    /// keeps its own send; by time, with a send lost, the first two are
    /// taken as the third's and the repeats left out.
    #[test]
    fn events_held_up_past_the_next_send_pair_by_order() {
        let event = |arrival| Shown {
            number: 0,
            arrival,
            swap: 0,
            ust: 0,
            msc: 0,
            frames: None,
        };
        let shown = [event(350), event(351), event(352)];
        let (paired, by, repeated) = pairs(&[100, 200, 300], &shown);
        assert_eq!(by, "order");
        assert_eq!(
            paired.iter().map(|(sent, _)| *sent).collect::<Vec<_>>(),
            [100, 200, 300]
        );
        assert_eq!(repeated, 0);
        let (paired, by, repeated) = pairs(&[100, 200, 300, 400], &shown);
        assert_eq!((by, paired.len(), repeated), ("time", 1, 2));
    }

    #[test]
    fn percentiles_are_by_nearest_rank() {
        let values: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50), Some(50));
        assert_eq!(percentile(&values, 99), Some(99));
        assert_eq!(percentile(&[7_i64], 99), Some(7));
        assert_eq!(percentile::<i64>(&[], 50), None);
        assert_eq!(millis(-1_500_000), "-1.500");
    }
}
