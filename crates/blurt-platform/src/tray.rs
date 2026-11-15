//! The system tray icon: blurt's state at a glance, and a menu to show the
//! window, mute, deafen or quit while the window is closed.
//!
//! [`listen`] puts the icon in the tray and reports what the user picks, as
//! a stream for the app's event loop; [`Controller::set`] updates what it
//! shows. The app draws the icon itself ([`Image`]), so this knows nothing
//! about how blurt looks.
//!
//! Linux uses StatusNotifierItem over D-Bus (through ksni): KDE Plasma,
//! and most other desktops with a tray (GNOME needs the AppIndicator
//! extension).

use futures_channel::mpsc;
use futures_util::{Stream, StreamExt, stream};

use crate::Action;

/// An icon image, as the tray takes it: ARGB32, big-endian (A, R, G, B
/// bytes in order), row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub argb: Vec<u8>,
}

/// What the tray shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The icon, at a few sizes for the desktop to pick from.
    pub icon: Vec<Image>,
    /// A line for the tooltip under blurt's name, e.g. the server.
    pub status: String,
    /// Whether there's a connection to mute or deafen.
    pub connected: bool,
    pub muted: bool,
    pub deafened: bool,
    /// Whether the window is open, for "Show" or "Hide".
    pub window_open: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The first event: how to update the tray.
    Ready(Controller),
    /// The icon is in the tray.
    Shown,
    /// The user picked something from the menu, or clicked the icon
    /// ([`Action::ShowWindow`], to show or hide the window).
    Action(Action),
    /// There's no tray to show the icon in, or it went away. The stream
    /// ends after this.
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

/// Puts blurt in the tray, showing `state`, and reports its use. `app_id`
/// is the app's desktop file name. Must be polled on a tokio runtime.
pub fn listen(app_id: &str, state: State) -> impl Stream<Item = Event> + use<> {
    let (events, out) = mpsc::unbounded();
    let (updates, inbox) = mpsc::unbounded();
    let _ = events.unbounded_send(Event::Ready(Controller(updates)));
    let app_id = app_id.to_owned();
    let driver = async move {
        if let Err(e) = imp::run(&app_id, state, &events, inbox).await {
            log::warn!("No tray icon: {e}");
            let _ = events.unbounded_send(Event::Unavailable(e));
        }
    };
    // The driver produces nothing itself; it runs while `out` is read.
    stream::select(out, stream::once(driver).filter_map(|()| async { None }))
}

/// The menu's items, platform-neutral, so they can be tested.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Item {
    Button {
        label: &'static str,
        action: Action,
    },
    Check {
        label: &'static str,
        checked: bool,
        enabled: bool,
        action: Action,
    },
    Separator,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn menu(state: &State) -> Vec<Item> {
    vec![
        Item::Button {
            label: if state.window_open {
                "Hide blurt"
            } else {
                "Show blurt"
            },
            action: Action::ShowWindow,
        },
        Item::Separator,
        Item::Check {
            label: "Mute",
            // Deafening mutes too.
            checked: state.muted || state.deafened,
            enabled: state.connected,
            action: Action::ToggleMute,
        },
        Item::Check {
            label: "Deafen",
            checked: state.deafened,
            enabled: state.connected,
            action: Action::ToggleDeafen,
        },
        Item::Separator,
        Item::Button {
            label: "Quit",
            action: Action::Quit,
        },
    ]
}

#[cfg(target_os = "linux")]
mod imp {
    use futures_channel::mpsc;
    use futures_util::StreamExt;
    use ksni::TrayMethods;
    use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};

    use super::{Event, Item, State, menu};
    use crate::Action;

    struct Tray {
        app_id: String,
        state: State,
        events: mpsc::UnboundedSender<Event>,
    }

    impl Tray {
        fn send(&self, action: Action) {
            let _ = self.events.unbounded_send(Event::Action(action));
        }
    }

    impl ksni::Tray for Tray {
        fn id(&self) -> String {
            self.app_id.clone()
        }

        fn title(&self) -> String {
            "blurt".into()
        }

        fn category(&self) -> ksni::Category {
            ksni::Category::Communications
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.state
                .icon
                .iter()
                .map(|image| ksni::Icon {
                    width: image.width as i32,
                    height: image.height as i32,
                    data: image.argb.clone(),
                })
                .collect()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: "blurt".into(),
                description: self.state.status.clone(),
                ..Default::default()
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(Action::ShowWindow);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            menu(&self.state)
                .into_iter()
                .map(|item| match item {
                    Item::Button { label, action } => StandardItem {
                        label: label.into(),
                        activate: Box::new(move |tray: &mut Self| tray.send(action)),
                        ..Default::default()
                    }
                    .into(),
                    Item::Check {
                        label,
                        checked,
                        enabled,
                        action,
                    } => CheckmarkItem {
                        label: label.into(),
                        checked,
                        enabled,
                        activate: Box::new(move |tray: &mut Self| tray.send(action)),
                        ..Default::default()
                    }
                    .into(),
                    Item::Separator => MenuItem::Separator,
                })
                .collect()
        }

        fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
            log::info!("The tray went away: {reason:?}");
            // Keep the service: the tray usually comes back (e.g. the
            // panel restarting), and ksni registers again when it does.
            true
        }
    }

    pub async fn run(
        app_id: &str,
        state: State,
        events: &mpsc::UnboundedSender<Event>,
        mut inbox: mpsc::UnboundedReceiver<State>,
    ) -> Result<(), String> {
        let tray = Tray {
            app_id: app_id.to_owned(),
            state,
            events: events.clone(),
        };
        // In a sandbox (Flatpak), only the portal-style unnamed service
        // gets through.
        let sandboxed = std::path::Path::new("/.flatpak-info").exists();
        let handle = tray
            .disable_dbus_name(sandboxed)
            .spawn()
            .await
            .map_err(|e| e.to_string())?;
        let _ = events.unbounded_send(Event::Shown);
        while let Some(mut state) = inbox.next().await {
            // Only the latest matters.
            while let Ok(newer) = inbox.try_recv() {
                state = newer;
            }
            if handle.update(|tray| tray.state = state).await.is_none() {
                return Err("the tray service stopped".into());
            }
        }
        handle.shutdown().await;
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use futures_channel::mpsc;

    use super::{Event, State};

    pub async fn run(
        _app_id: &str,
        _state: State,
        _events: &mpsc::UnboundedSender<Event>,
        _inbox: mpsc::UnboundedReceiver<State>,
    ) -> Result<(), String> {
        Err("a tray icon isn't supported on this platform yet".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(state: &State) -> Vec<String> {
        menu(state)
            .iter()
            .map(|item| match item {
                Item::Button { label, .. } => label.to_string(),
                Item::Check {
                    label,
                    checked,
                    enabled,
                    ..
                } => format!(
                    "{label}{}{}",
                    if *checked { " ✓" } else { "" },
                    if *enabled { "" } else { " (off)" }
                ),
                Item::Separator => "-".into(),
            })
            .collect()
    }

    #[test]
    fn menu_follows_the_state() {
        let idle = State::default();
        assert_eq!(
            labels(&idle),
            ["Show blurt", "-", "Mute (off)", "Deafen (off)", "-", "Quit"]
        );
        let deafened = State {
            connected: true,
            deafened: true,
            window_open: true,
            ..State::default()
        };
        assert_eq!(
            labels(&deafened),
            ["Hide blurt", "-", "Mute ✓", "Deafen ✓", "-", "Quit"]
        );
    }
}
