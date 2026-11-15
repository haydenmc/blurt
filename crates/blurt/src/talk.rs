//! Who is talking, and who talked recently: the "talking line" under the
//! channel name.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long someone stays in the "talking" group after going silent, so it
/// doesn't flicker between words.
pub const HOLD: Duration = Duration::from_millis(1500);
/// The most names the line shows.
pub const MAX: usize = 6;

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// When they started talking, or (while silent) when they stopped.
    since: Instant,
    talking: bool,
}

/// Talking state per session, local to this channel visit.
#[derive(Debug, Default)]
pub struct TalkLog {
    entries: HashMap<u32, Entry>,
}

/// One name on the talking line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spoke {
    pub session: u32,
    /// In the "talking now" group: talking, or stopped within [`HOLD`].
    /// Whether they are actually talking is [`TalkLog::speaking`].
    pub talking: bool,
    /// How long since they stopped; `None` while talking.
    pub ago: Option<Duration>,
}

impl TalkLog {
    /// Records whether `session` is talking. Cheap to call with unchanged
    /// state; only changes are recorded.
    pub fn observe(&mut self, session: u32, talking: bool, now: Instant) {
        match self.entries.get_mut(&session) {
            Some(entry) if entry.talking == talking => {}
            Some(entry) => {
                // Starting again inside the hold keeps their place.
                if !(talking && now.duration_since(entry.since) < HOLD) {
                    entry.since = now;
                }
                entry.talking = talking;
            }
            None if talking => {
                self.entries.insert(
                    session,
                    Entry {
                        since: now,
                        talking: true,
                    },
                );
            }
            None => {}
        }
    }

    /// Talking at this moment; no hold.
    pub fn speaking(&self, session: u32) -> bool {
        self.entries.get(&session).is_some_and(|e| e.talking)
    }

    /// Forgets everyone; e.g. on changing channel.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Forgets anyone not `present`, e.g. who left the channel.
    pub fn retain(&mut self, present: impl Fn(u32) -> bool) {
        self.entries.retain(|&session, _| present(session));
    }

    /// The line: people talking first, then the rest, newest first.
    pub fn line(&self, now: Instant) -> Vec<Spoke> {
        let mut line: Vec<(Instant, Spoke)> = self
            .entries
            .iter()
            .map(|(&session, e)| {
                let talking = e.talking || now.duration_since(e.since) < HOLD;
                let ago = (!talking).then(|| now.duration_since(e.since));
                (
                    e.since,
                    Spoke {
                        session,
                        talking,
                        ago,
                    },
                )
            })
            .collect();
        line.sort_by(|(a_since, a), (b_since, b)| {
            b.talking
                .cmp(&a.talking)
                .then(b_since.cmp(a_since))
                .then(a.session.cmp(&b.session))
        });
        line.into_iter().take(MAX).map(|(_, s)| s).collect()
    }
}

/// `8s`, `41s`, `3m`, `1h`.
pub fn elapsed(ago: Duration) -> String {
    let s = ago.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn nobody_yet_is_an_empty_line() {
        let log = TalkLog::default();
        assert!(log.line(Instant::now()).is_empty());
        // Going quiet without ever talking isn't news.
        let mut log = TalkLog::default();
        log.observe(1, false, Instant::now());
        assert!(log.line(Instant::now()).is_empty());
    }

    #[test]
    fn talking_holds_between_words() {
        let t0 = Instant::now();
        let mut log = TalkLog::default();
        log.observe(1, true, t0);
        log.observe(1, false, at(t0, 1000));
        assert!(!log.speaking(1), "colour follows talking at once");
        assert!(log.line(at(t0, 2000))[0].talking, "but the place is held");
        assert_eq!(log.line(at(t0, 2000))[0].ago, None);
        assert!(!log.line(at(t0, 2600))[0].talking);
        assert_eq!(
            log.line(at(t0, 9000))[0].ago,
            Some(Duration::from_millis(8000))
        );
    }

    #[test]
    fn talking_first_then_newest() {
        let t0 = Instant::now();
        let mut log = TalkLog::default();
        // 1 talked first, then 2; 3 is talking now.
        log.observe(1, true, t0);
        log.observe(1, false, at(t0, 100));
        log.observe(2, true, at(t0, 200));
        log.observe(2, false, at(t0, 300));
        log.observe(3, true, at(t0, 5000));
        let line = log.line(at(t0, 6000));
        let order: Vec<u32> = line.iter().map(|s| s.session).collect();
        assert_eq!(order, [3, 2, 1]);
        assert!(line[0].talking && !line[1].talking);
    }

    #[test]
    fn at_most_six() {
        let t0 = Instant::now();
        let mut log = TalkLog::default();
        for session in 0..10 {
            log.observe(session, true, at(t0, u64::from(session)));
        }
        let line = log.line(at(t0, 100));
        assert_eq!(line.len(), MAX);
        // The newest six.
        assert_eq!(line[0].session, 9);
        assert_eq!(line[5].session, 4);
    }

    #[test]
    fn elapsed_labels() {
        let s = Duration::from_secs;
        assert_eq!(elapsed(s(8)), "8s");
        assert_eq!(elapsed(s(41)), "41s");
        assert_eq!(elapsed(s(200)), "3m");
        assert_eq!(elapsed(s(3700)), "1h");
    }
}
