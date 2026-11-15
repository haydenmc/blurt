//! Desktop integration for blurt: global hotkeys (and, later, the system
//! tray), behind platform-neutral types so the app doesn't see which
//! desktop API is underneath.
//!
//! Linux uses the XDG GlobalShortcuts portal (through ashpd), so the
//! desktop (KDE, GNOME 48+, Hyprland...) owns the key bindings: it asks the
//! user to approve them and lets them rebind.

pub mod hotkeys;

/// Something the user can trigger from a global hotkey or the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    PushToTalk,
    ShowWindow,
}
