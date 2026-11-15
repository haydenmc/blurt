//! Global hotkeys: shortcuts that work while blurt isn't focused.
//!
//! [`listen`] binds the shortcuts and reports them being pressed, as a
//! stream for the app's event loop. The desktop decides the actual keys:
//! blurt only suggests some, and the user approves or changes them in the
//! desktop's own settings (which [`Controller::configure`] can open).

use futures_channel::mpsc;
use futures_util::{Stream, StreamExt, stream};

use crate::Action;

/// A shortcut blurt offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub action: Action,
    /// Stable, for the desktop to remember the user's binding by.
    pub id: &'static str,
    /// What the desktop shows the user.
    pub description: &'static str,
    /// The keys blurt suggests, in the XDG shortcuts format
    /// (e.g. `CTRL+ALT+m`). The desktop may ignore it.
    pub suggested: &'static str,
}

/// The shortcuts blurt binds.
pub const SHORTCUTS: [Shortcut; 2] = [
    Shortcut {
        action: Action::ToggleMute,
        id: "toggle-mute",
        description: "Mute or unmute your microphone",
        suggested: "CTRL+ALT+m",
    },
    Shortcut {
        action: Action::ToggleDeafen,
        id: "toggle-deafen",
        description: "Deafen or undeafen",
        suggested: "CTRL+ALT+d",
    },
];

/// What an action is bound to now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub action: Action,
    /// The desktop's description of the keys, e.g. "Ctrl+Alt+M"; `None` if
    /// the user hasn't bound any.
    pub keys: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The first event: how to ask for things later.
    Ready(Controller),
    /// The shortcuts are bound (or the user changed them).
    Bound {
        bindings: Vec<Binding>,
        /// Whether [`Controller::configure`] can open the desktop's
        /// settings for them.
        configurable: bool,
    },
    Pressed(Action),
    Released(Action),
    /// No global hotkeys: the desktop has no shortcuts portal, or it
    /// refused. The stream ends after this.
    Unavailable(String),
}

/// Requests to the running [`listen`].
#[derive(Debug, Clone)]
pub struct Controller(mpsc::UnboundedSender<Command>);

#[derive(Debug)]
enum Command {
    Configure,
}

impl Controller {
    /// Opens the desktop's settings for blurt's shortcuts, where it has
    /// them (see [`Event::Bound`]).
    pub fn configure(&self) {
        let _ = self.0.unbounded_send(Command::Configure);
    }
}

/// Binds [`SHORTCUTS`] for the app `app_id` (its desktop file's name, for
/// the desktop to show and remember it by) and reports their use. Must be
/// polled on a tokio runtime.
pub fn listen(app_id: &str) -> impl Stream<Item = Event> + use<> {
    let (events, out) = mpsc::unbounded();
    let (commands, inbox) = mpsc::unbounded();
    let _ = events.unbounded_send(Event::Ready(Controller(commands)));
    let app_id = app_id.to_owned();
    let driver = async move {
        if let Err(e) = imp::run(&app_id, &events, inbox).await {
            log::warn!("No global hotkeys: {e}");
            let _ = events.unbounded_send(Event::Unavailable(e));
        }
    };
    // The driver produces nothing itself; it runs while `out` is read.
    stream::select(out, stream::once(driver).filter_map(|()| async { None }))
}

fn shortcut(id: &str) -> Option<&'static Shortcut> {
    SHORTCUTS.iter().find(|s| s.id == id)
}

#[cfg(target_os = "linux")]
mod imp {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_channel::mpsc;
    use futures_util::{StreamExt, stream};

    use super::{Binding, Command, Event, SHORTCUTS, shortcut};

    enum Input {
        Pressed(String),
        Released(String),
        Changed(Vec<Binding>),
        Command(Command),
    }

    fn bindings<'a>(
        bound: impl IntoIterator<Item = &'a ashpd::desktop::global_shortcuts::Shortcut>,
    ) -> Vec<Binding> {
        let bound: Vec<_> = bound.into_iter().collect();
        SHORTCUTS
            .iter()
            .map(|s| Binding {
                action: s.action,
                keys: bound
                    .iter()
                    .find(|b| b.id() == s.id)
                    .map(|b| b.trigger_description().trim().to_owned())
                    .filter(|keys| !keys.is_empty()),
            })
            .collect()
    }

    pub async fn run(
        app_id: &str,
        events: &mpsc::UnboundedSender<Event>,
        inbox: mpsc::UnboundedReceiver<Command>,
    ) -> Result<(), String> {
        // Outside a sandbox, the portal can't tell which app is asking;
        // tell it, so it can show blurt's name and remember the bindings.
        // Older portals don't have the registry; carry on without it.
        match ashpd::AppID::try_from(app_id) {
            Ok(id) => {
                if let Err(e) = ashpd::register_host_app(id).await {
                    log::debug!("Could not register with the portals: {e}");
                }
            }
            Err(e) => log::warn!("{app_id:?} isn't an app ID: {e}"),
        }
        let portal = GlobalShortcuts::new().await.map_err(|e| e.to_string())?;
        let session = portal
            .create_session(Default::default())
            .await
            .map_err(|e| e.to_string())?;
        let new: Vec<NewShortcut> = SHORTCUTS
            .iter()
            .map(|s| NewShortcut::new(s.id, s.description).preferred_trigger(s.suggested))
            .collect();
        let bound = portal
            .bind_shortcuts(&session, &new, None, Default::default())
            .await
            .and_then(|request| request.response())
            .map_err(|e| e.to_string())?;
        let configurable = portal.version() >= 2;
        let _ = events.unbounded_send(Event::Bound {
            bindings: bindings(bound.shortcuts()),
            configurable,
        });
        log::info!("Global hotkeys bound: {:?}", bindings(bound.shortcuts()));

        let pressed = portal
            .receive_activated()
            .await
            .map_err(|e| e.to_string())?
            .map(|a| Input::Pressed(a.shortcut_id().to_owned()));
        let released = portal
            .receive_deactivated()
            .await
            .map_err(|e| e.to_string())?
            .map(|d| Input::Released(d.shortcut_id().to_owned()));
        let changed = portal
            .receive_shortcuts_changed()
            .await
            .map_err(|e| e.to_string())?
            .map(|c| Input::Changed(bindings(c.shortcuts())));
        let mut inputs = stream::select(
            stream::select(pressed, released),
            stream::select(changed, inbox.map(Input::Command)),
        );
        while let Some(input) = inputs.next().await {
            let event = match input {
                Input::Pressed(id) => shortcut(&id).map(|s| Event::Pressed(s.action)),
                Input::Released(id) => shortcut(&id).map(|s| Event::Released(s.action)),
                Input::Changed(bindings) => Some(Event::Bound {
                    bindings,
                    configurable,
                }),
                Input::Command(Command::Configure) => {
                    if let Err(e) = portal
                        .configure_shortcuts(&session, None, Default::default())
                        .await
                    {
                        log::warn!("Could not open the shortcut settings: {e}");
                    }
                    None
                }
            };
            if let Some(event) = event
                && events.unbounded_send(event).is_err()
            {
                // Nobody's listening any more.
                break;
            }
        }
        let _ = session.close().await;
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use futures_channel::mpsc;

    use super::{Command, Event};

    pub async fn run(
        _app_id: &str,
        _events: &mpsc::UnboundedSender<Event>,
        _inbox: mpsc::UnboundedReceiver<Command>,
    ) -> Result<(), String> {
        Err("global hotkeys aren't supported on this platform yet".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_ids_are_unique_and_found() {
        for s in &SHORTCUTS {
            assert_eq!(shortcut(s.id).map(|f| f.action), Some(s.action));
        }
        assert_eq!(shortcut("nope"), None);
    }
}
