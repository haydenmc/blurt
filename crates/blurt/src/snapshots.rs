//! Pictures of the UI with made-up state, for checking the design by eye.
//!
//! ```sh
//! BLURT_SNAPSHOTS=/tmp/shots cargo test -p blurt snapshots -- --ignored
//! ```
//!
//! writes one PNG per screen and theme (at 2× scale) to that directory.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::Utc;
use iced::widget::{column, container};
use iced::{Element, Fill, Size};
use mumble_protocol::state::{Channel, ChatTextMessage, MumbleState, TextMessage, User};

use crate::Message;
use crate::config::{Config, Server};
use crate::room::Room;
use crate::theme::{self, Accent, Mode, Tokens};
use crate::ui::voice::{self, Voice};
use crate::ui::{Layout, Page, Pivot, chrome, pages};

fn server() -> Server {
    Server {
        label: "Nightjar".into(),
        host: "voice.example.org".into(),
        port: 64738,
        username: "me".into(),
        password: None,
    }
}

fn channel(id: u32, parent: Option<u32>, name: &str, locked: bool) -> Channel {
    Channel {
        channel_id: id,
        parent,
        name: name.into(),
        is_enter_restricted: locked,
        can_enter: !locked,
        ..Default::default()
    }
}

fn user(session: u32, name: &str, channel_id: u32) -> User {
    User {
        session,
        name: name.into(),
        channel_id,
        ..Default::default()
    }
}

/// A server with a few channels and people, some talking, some muted.
fn room(now: Instant) -> Room {
    let mut state = MumbleState::default();
    for c in [
        channel(0, None, "nightjar", false),
        channel(1, Some(0), "lobby", false),
        channel(2, Some(0), "late night", false),
        channel(3, Some(0), "games", true),
        channel(4, Some(3), "raid", false),
        channel(5, Some(0), "afk", false),
    ] {
        state.channels.insert(c.channel_id, c);
    }
    let mut me = user(1, "me", 2);
    me.is_me = true;
    let mut kestrel = user(2, "kestrel", 2);
    kestrel.is_speaking = true;
    let mut wren = user(3, "wren", 2);
    wren.self_mute = true;
    let mut heron = user(4, "heron", 2);
    heron.self_deaf = true;
    heron.self_mute = true;
    for u in [
        me,
        kestrel,
        wren,
        heron,
        user(5, "plover", 2),
        user(6, "dunlin", 1),
        user(7, "curlew", 4),
        user(8, "godwit", 4),
        user(9, "snipe", 5),
    ] {
        state.users.insert(u.session, u);
    }
    state.current_user_session = Some(1);

    let mut room = Room::new(server());
    let earlier = |s| now - Duration::from_secs(s);
    // Someone arrives while you're there.
    let mut before = state.clone();
    before.users.remove(&5);
    room.update(before, earlier(60));
    let said = |actor, text: &str, mins: i64| {
        TextMessage::ChatTextMessage(ChatTextMessage {
            timestamp: Utc::now() - chrono::Duration::minutes(mins),
            actor: Some(actor),
            session: vec![],
            content: text.into(),
        })
    };
    state.chat_messages = vec![
        said(2, "anyone up for a round later?", 14),
        said(3, "after dinner. i'm on mute, kids are asleep", 12),
        said(4, "deafened for a bit — call me if you need me", 9),
    ];
    room.update(state, now);
    // plover talked 41 s ago, wren 8 s ago; kestrel is talking now.
    room.talk.observe(5, true, earlier(50));
    room.talk.observe(5, false, earlier(41));
    room.talk.observe(3, true, earlier(12));
    room.talk.observe(3, false, earlier(8));
    room.pivot = Pivot::People;
    room
}

/// The window as the app draws it: `header` in the title bar over `body`.
fn window<'a>(
    t: Tokens,
    header: Element<'a, Message>,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    chrome::with_resize_edges(
        container(column![chrome::title_bar(t, header), body])
            .width(Fill)
            .height(Fill)
            .style(theme::fill(t.bg))
            .into(),
    )
}

/// The voice screen's title bar contents, as `main` picks them.
fn header<'a>(t: Tokens, layout: Layout) -> Element<'a, Message> {
    match layout {
        Layout::Wide => voice::nav_row(t, Page::Voice, true, Some("Nightjar")),
        Layout::Compact => voice::compact_header(t, "Nightjar"),
    }
}

fn save(dir: &std::path::Path, name: &str, t: Tokens, size: Size, view: Element<'_, Message>) {
    let settings = iced::Settings {
        fonts: theme::FONTS.iter().map(|f| (*f).into()).collect(),
        default_font: theme::ui(iced::font::Weight::Normal),
        ..Default::default()
    };
    let mut simulator = iced_test::Simulator::with_size(settings, size, view);
    let snapshot = simulator.snapshot(&t.iced_theme()).expect("snapshot");
    let path = dir.join(name);
    // `matches_image` keeps an existing picture; start fresh each run. It
    // names files after the renderer.
    for renderer in ["tiny-skia", "wgpu"] {
        let _ = std::fs::remove_file(dir.join(format!("{name}-{renderer}.png")));
    }
    snapshot.matches_image(&path).expect("write snapshot");
}

#[test]
#[ignore = "writes pictures; run with BLURT_SNAPSHOTS set"]
fn snapshots() {
    let Some(dir) = std::env::var_os("BLURT_SNAPSHOTS").map(PathBuf::from) else {
        return;
    };
    let now = Instant::now();
    let mut room = room(now);
    // Past the colour transitions, with you talking.
    let later = now + Duration::from_secs(1);
    room.transmitting = true;
    room.mic_level = 0.2;
    // A voice in the bands (lowest first), as the engine reports it.
    room.spectrum.playback = [
        0.03, 0.05, 0.08, 0.09, 0.07, 0.06, 0.045, 0.035, 0.028, 0.02, 0.016, 0.012, 0.009, 0.006,
        0.004, 0.002,
    ];
    room.spectrum.mic = room.spectrum.playback.map(|b| b * 0.6);
    room.levels = vec![(2, 0.15)];
    room.observe_talking(now);
    // Settled on that sound.
    for ms in (0..1000).step_by(16) {
        room.step_glow(now + Duration::from_millis(ms));
    }

    let hotkeys = crate::Hotkeys {
        bindings: vec![
            blurt_platform::hotkeys::Binding {
                action: blurt_platform::Action::ToggleMute,
                keys: Some("Ctrl+Alt+M".into()),
            },
            blurt_platform::hotkeys::Binding {
                action: blurt_platform::Action::ToggleDeafen,
                keys: None,
            },
        ],
        configurable: true,
        ..Default::default()
    };
    let config = Config {
        servers: vec![
            server(),
            Server {
                label: "Work".into(),
                host: "mumble.internal".into(),
                ..server()
            },
        ],
        ..Default::default()
    };

    for (mode, accent, tag) in [
        (Mode::Dark, Accent::Zune, "dark"),
        (Mode::Light, Accent::Zune, "light"),
        (Mode::Dark, Accent::Ice, "dark-ice"),
    ] {
        let t = Tokens::new(mode, accent);
        for (layout, size, name) in [
            (Layout::Wide, Size::new(1100.0, 720.0), "wide"),
            (Layout::Wide, Size::new(730.0, 600.0), "wide-narrow"),
            (Layout::Compact, Size::new(420.0, 820.0), "compact"),
        ] {
            let v = Voice {
                t,
                room: &room,
                now: later,
                width: size.width,
                layout,
                reduce_motion: false,
                volume: 0.8,
                glow: true,
            };
            save(
                &dir,
                &format!("voice-{name}-{tag}"),
                t,
                size,
                window(t, header(t, layout), voice::view(&v)),
            );
        }
        if tag == "dark" {
            for pivot in [Pivot::Chat, Pivot::Channels] {
                room.pivot = pivot;
                let size = Size::new(420.0, 820.0);
                let v = Voice {
                    t,
                    room: &room,
                    now: later,
                    width: size.width,
                    layout: Layout::Compact,
                    reduce_motion: false,
                    volume: 0.8,
                    glow: true,
                };
                save(
                    &dir,
                    &format!("voice-compact-{}", pivot.name()),
                    t,
                    size,
                    window(t, header(t, Layout::Compact), voice::view(&v)),
                );
            }
            room.pivot = Pivot::People;
            let size = Size::new(900.0, 600.0);
            let nav = |page| voice::nav_row(t, page, true, Some("Nightjar"));
            save(
                &dir,
                "servers",
                t,
                size,
                window(
                    t,
                    nav(Page::Servers),
                    pages::servers(t, &config, Some(&config.servers[0])),
                ),
            );
            save(
                &dir,
                "settings",
                t,
                size,
                window(
                    t,
                    nav(Page::Settings),
                    pages::settings(t, &config, false, &hotkeys),
                ),
            );
        }
    }
}
