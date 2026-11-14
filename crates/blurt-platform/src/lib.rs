//! Desktop integration for blurt: system tray and global hotkeys.
//!
//! Linux uses StatusNotifierItem (ksni) and the XDG GlobalShortcuts portal
//! (ashpd). Implemented in milestone 5.

/// Something the user can trigger from a global hotkey or the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    PushToTalk,
    ShowWindow,
}
