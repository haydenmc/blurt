//! The call, as the system sees it: on Windows 11, the taskbar's
//! microphone button and its shortcut (Win+Alt+K), "universal mute".
//!
//! [`listen`] reports the system asking blurt to mute or unmute;
//! [`Controller::set`] tells it whether blurt is in a call, and whether
//! it's muted. Nothing here takes keys from other apps, so it needs no
//! setting.
//!
//! Windows uses WinRT's `VoipCallCoordinator`, which works without
//! package identity (as citelao's universal mute plugin for Mumble
//! showed). Other platforms have no such thing.

use futures_channel::mpsc;
use futures_util::{Stream, StreamExt, stream};

/// What the system is told.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The call's name (e.g. the server), or `None` while there's none.
    pub call: Option<String>,
    /// Whether the microphone is off, deafened included.
    pub muted: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The first event: how to tell the system things.
    Ready(Controller),
    /// The user asked the system to mute (`true`) or unmute blurt. The
    /// system waits to hear the outcome: set the state again, even if
    /// nothing changed.
    MuteRequested(bool),
    /// The system has nowhere to show the call. The stream ends after
    /// this.
    Unavailable(String),
}

/// Updates the running [`listen`].
#[derive(Debug, Clone)]
pub struct Controller(mpsc::UnboundedSender<State>);

impl Controller {
    pub fn set(&self, state: State) {
        let _ = self.0.unbounded_send(state);
    }
}

/// Reports the system's mute requests, and tells it about the call. Ends
/// any call when the stream is dropped.
pub fn listen() -> impl Stream<Item = Event> {
    let (events, out) = mpsc::unbounded();
    let (updates, inbox) = mpsc::unbounded();
    let _ = events.unbounded_send(Event::Ready(Controller(updates)));
    let driver = async move {
        if let Err(e) = imp::run(&events, inbox).await {
            if cfg!(windows) {
                log::warn!("No universal mute: {e}");
            } else {
                log::debug!("No universal mute: {e}");
            }
            let _ = events.unbounded_send(Event::Unavailable(e));
        }
    };
    // The driver produces nothing itself; it runs while `out` is read.
    stream::select(out, stream::once(driver).filter_map(|()| async { None }))
}

/// What to tell the system, platform-neutral, so it can be tested.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Step {
    Start(String),
    End,
    Notify { muted: bool },
}

/// How to go from telling the system `now` to `next`. While in a call,
/// the mute state is always told again: it may answer a request.
#[cfg_attr(not(windows), allow(dead_code))]
fn steps(now: &State, next: &State) -> Vec<Step> {
    let mut steps = Vec::new();
    if now.call != next.call {
        if now.call.is_some() {
            steps.push(Step::End);
        }
        if let Some(name) = &next.call {
            steps.push(Step::Start(name.clone()));
        }
    }
    if next.call.is_some() {
        steps.push(Step::Notify { muted: next.muted });
    }
    steps
}

#[cfg(windows)]
mod win32;
#[cfg(windows)]
use win32 as imp;

#[cfg(not(windows))]
mod imp {
    use futures_channel::mpsc;

    use super::{Event, State};

    pub async fn run(
        _events: &mpsc::UnboundedSender<Event>,
        _inbox: mpsc::UnboundedReceiver<State>,
    ) -> Result<(), String> {
        Err("only Windows has it".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_call(name: &str, muted: bool) -> State {
        State {
            call: Some(name.into()),
            muted,
        }
    }

    #[test]
    fn a_call_starts_then_says_whether_its_muted() {
        assert_eq!(
            steps(&State::default(), &in_call("home", true)),
            [Step::Start("home".into()), Step::Notify { muted: true }]
        );
    }

    #[test]
    fn mute_is_told_again_even_unchanged() {
        let now = in_call("home", false);
        assert_eq!(steps(&now, &now), [Step::Notify { muted: false }]);
        assert_eq!(
            steps(&now, &in_call("home", true)),
            [Step::Notify { muted: true }]
        );
    }

    #[test]
    fn another_server_is_another_call() {
        assert_eq!(
            steps(&in_call("home", false), &in_call("work", false)),
            [
                Step::End,
                Step::Start("work".into()),
                Step::Notify { muted: false }
            ]
        );
    }

    #[test]
    fn leaving_ends_the_call_and_nothing_else() {
        assert_eq!(
            steps(&in_call("home", true), &State::default()),
            [Step::End]
        );
        assert!(steps(&State::default(), &State::default()).is_empty());
    }
}
