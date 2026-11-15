//! Desktop integration for blurt: global hotkeys and the system tray,
//! behind platform-neutral types so the app doesn't see which desktop API
//! is underneath.
//!
//! Linux uses the XDG GlobalShortcuts portal (through ashpd), so the
//! desktop (KDE, GNOME 48+, Hyprland...) owns the key bindings: it asks the
//! user to approve them and lets them rebind. The tray icon is a
//! StatusNotifierItem (through ksni). Both share one D-Bus library (zbus).

pub mod hotkeys;
pub mod tray;

/// Something the user can trigger from a global hotkey or the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    PushToTalk,
    /// Show the window, or hide it if it's showing.
    ShowWindow,
    Quit,
}
