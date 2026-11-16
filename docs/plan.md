# blurt: project plan

## Context
The goal is a native Mumble client where speed, a small footprint, low and clean audio latency, and usability come first. Linux on Wayland comes first, and Windows/macOS should stay possible later. Requirements: a minimal UI in the style of Halloy, native Wayland, global hotkeys (Mute/Deafen/PTT), and minimize-to-tray.

Decisions made:
- **UI: iced 0.14** (same as Halloy).
- **Protocol:** fork `crates/mumble-protocol` from citelao/mumbo (commit `2740157`) into this workspace, keep attribution, and **license blurt AGPL-3.0-or-later**. Send fixes upstream where they make sense.

What mumbo gave us at the fork:
- `crates/mumble-protocol`: prost-generated `Mumble.proto`/`MumbleUDP.proto`, a TCP framing codec (`codec.rs`), `ServerToClientMessage`/`ClientToServerMessage` enums (`lib.rs`), a state reducer `MumbleState::apply` that emits `MumbleEvent`s (`state.rs`), and `connect()`/`MumbleClient` with mute, channel switching, text, and send_audio (`client.rs`).
- What it's missing: **voice only goes through the TCP tunnel** (no UDP and no OCB2-AES128 crypto); it accepts any cert and has **no client-cert auth**; unknown message types end the connection; `unwrap()`s in the reader path; state sits behind `Arc<Mutex<>>`.
- `src-tauri/src/audio.rs` is a good reference for the audio stack: cpal + opus + neteq (jitter buffer) + rubato + **sonora** (pure-Rust WebRTC audio processing: AEC3 echo cancellation, AGC2 gain) + **nnnoiseless** (RNNoise). Its TODO list names the mistakes to avoid: mixing runs on the network task, NetEq ignores `frame_number`, and speakers are never cleaned up.

## Architecture

Cargo workspace (resolver 3, edition 2024; release profile uses lto, codegen-units=1, panic=abort, strip):

```
crates/mumble-protocol/   forked; protocol + connection, no UI or audio
crates/blurt-audio/       real-time audio engine (capture → DSP → encode, decode → jitter → mix → play)
crates/blurt-platform/    tray + global hotkeys behind traits, with per-OS backends
crates/blurt/             iced app (bin): UI, config, wiring
dev/mumble-server/        compose.yaml for a local murmur (copied from mumbo)
```

### 1. mumble-protocol (forked and hardened)
- Add `crypt.rs`: OCB2-AES128 `CryptState` (encrypt/decrypt, good/late/lost/resync counters, nonce resync through `CryptSetup`), built on the `aes` crate. Test it against the vectors in Mumble's `src/tests/TestCrypt`.
- Add `udp.rs`: a UDP voice socket that sends encrypted pings. It uses UDP once pings come back and falls back to the TCP tunnel automatically. Support both the protobuf UDP format (1.5+) and the legacy format for older servers.
- TLS: verify the server cert with trust-on-first-use: pin its fingerprint per server in config and prompt when it changes. Add **client certificate auth** using a cert generated with `rcgen` on first run.
- Decode unrecognized messages as `Unknown { id, bytes }` instead of failing, and replace every `unwrap` on the reader path with logging.
- Separate the two data paths. Voice packets go through a bounded **SPSC channel straight to the audio engine**, and control events go through an unbounded channel to the UI. The UI never handles audio.
- Missing client messages to add: deafen, `VoiceTarget` (needed for whisper/shout later), `PermissionQuery`, `UserStats`, `RequestBlob`, and channel join with a password (access tokens).

### 2. blurt-audio (the latency-critical part)
Threads: the cpal input callback, the cpal output callback, and one **DSP thread** that runs at high priority (rtkit / `SCHED_FIFO` where allowed). Nothing in a callback allocates or locks: callbacks only push to or pop from `rtrb` ring buffers.

- **Capture path (on the DSP thread, in 10 ms frames):** input ring → resample to 48 kHz mono with rubato (skipped when the device is already 48k) → sonora `AudioProcessing` (AEC3 + AGC2, with the playback mix as the render reference) → nnnoiseless → VAD or PTT gate (with hangover) → Opus encode (VOIP mode, **10 ms frames by default**, 20 ms option, in-band FEC on, bitrate capped by the server's `max_bandwidth`) → network.
- **Playback path:** for each speaker session, a neteq jitter buffer with sequence/timestamp taken from Mumble's `frame_number`; talk spurts tracked through `is_terminator`, so concealment only runs inside a spurt → Opus decode with PLC/FEC → per-user volume and local mute → mix → copied to the AEC render reference → output ring. Speaker state is dropped on `UserRemove` and after idle time.
- Deafen stops decoding entirely. Mute stops capture processing at once, before the server confirms (mumbo's `SelfMuteStatus` pattern).
- Devices: cpal hosts. Inside the container this goes through ALSA → pulse plugin → host PipeWire. Measure the latency. If ALSA→pulse adds too much buffering, add a native PipeWire backend (`pipewire` crate) behind a feature flag.
- Notification sounds (join, leave, mute) are short, pre-decoded PCM clips mixed into the output.

### 3. blurt-platform
- Traits: `Tray { set_state(muted, deafened, talking); events() }` and `Hotkeys { bind(Action, Trigger); events() }`, where `Action` is Mute, Deafen, PushToTalk, or ShowWindow.
- **Linux tray:** `ksni` (StatusNotifierItem over D-Bus, pure Rust through zbus). Menu: Show/Hide, Mute, Deafen, Quit. The icon changes with mute/deafen state.
- **Linux global hotkeys:** the XDG **GlobalShortcuts portal** through `ashpd` (KDE, GNOME 48+, Hyprland). The compositor lets the user approve or rebind the shortcuts, and we get press and release events, so PTT works. The X11 fallback is the `global-hotkey` crate. When neither is available, the settings screen shows a CLI hint: `blurt --toggle-mute` sends to the running instance over D-Bus, so users can bind it in their compositor config.
- **Windows tray:** the notification area (`Shell_NotifyIconW`) through `windows-sys`, which the build already has, on a thread of its own with a hidden window. Same icon, tooltip and menu as on Linux; the icon comes back when Explorer restarts.
- Later: Windows global hotkeys, and macOS (the `cfg` backends are stubbed so the traits don't leak platform details).

### 4. blurt (iced app)
- Uses **`iced::daemon`**. Closing the window destroys it (freeing GPU and surface memory) while the process keeps running in the tray; "Show" opens a new window. This is what keeps the footprint small while minimized.
- Renderer: tiny-skia (CPU) by default: 19 MB RSS idle against 119 MB for wgpu on a software Vulkan driver, and a smaller binary. wgpu is the `gpu` feature, for a planned "rich visuals" setting (default on for machines with 32 GB+ RAM and a real GPU).
- Elm-style `App { state, update, view, subscription }`. Subscriptions combine protocol events, tray events, hotkey events, and a ~30 Hz "talking" tick that only runs while someone is talking.
- Views, keyboard-first and minimal (Halloy-like, with a sidebar):
  - Server list: favorites, saved credentials, and UDP ping, user count, and latency through `get_server_stats`.
  - Main view: a channel tree with users, talking/mute/deaf indicators, double-click to join, and a per-user volume menu. Plus a chat pane that sanitizes Mumble's HTML to plain or rich text and shows inline images up to `image_message_length`.
  - Settings: input/output device, VAD threshold with a live meter, PTT, NS/AEC/AGC toggles, frame size, hotkey bindings, theme.
  - Backlog: a transmit-mode setting with a strict push-to-talk mode (send only while the key or the pill is held). Today the engine runs voice activity, and holding push-to-talk also transmits.
- Config is TOML under `directories` config_dir. Passwords live in the OS keyring (`keyring`), and the client cert is stored next to the config.
- Allocator: mimalloc. Logging: `tracing`, with a ring buffer that the UI can show.

### Devcontainer change
Tray and portals need the host D-Bus session bus. Mount `${XDG_RUNTIME_DIR}/bus` and set `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus`. Without it, tray and hotkeys can't be tested from inside the container.

## Milestones
1. **Skeleton:** the workspace, the forked crate (with a NOTICE/attribution header), LICENSE (AGPL-3.0), CI (fmt, clippy -D warnings, test), the local murmur compose file, and the devcontainer D-Bus mount.
2. **Protocol:** hardening, OCB2 crypto, UDP with TCP fallback, cert auth, TOFU. Unit tests plus an integration test against the local murmur.
3. **Headless audio:** `blurt-audio`, plus a `blurt-cli` example binary that connects, joins a channel, and talks and listens. Latency and CPU measurements happen here, before any UI exists.
4. **UI:** an iced daemon covering server list → connect → channel tree → chat → settings.
5. **Desktop integration:** tray, GlobalShortcuts portal hotkeys (mute, deafen, PTT), close-to-tray, and single-instance with `--toggle-mute`.
6. **Polish:** reconnect with backoff, notification sounds, changing devices mid-call, Flathub submission (the Flatpak itself is built by CI since v0.1.0).

## Budgets (checked from milestone 3 on)
- Client-side mouth-to-wire under about 20 ms with 10 ms frames, and jitter-buffer target plus playout under about 40 ms on a good network.
- Idle CPU under 1.5% while connected and silent (relaxed from 1% on 2026-11-14 after measuring full AEC/NS/AGC on PipeWire at about 1.7%). RSS with the window closed (tray only) aims for under 40 MB. Release binary under about 21 MB (raised from 15 MB to fit bundled fonts and wgpu, then to 21 MB on 2026-11-15 for the tray, keeping speed over size in hot code).

## Verification
- `cargo test --workspace`: crypt vectors, codec round-trips, the state reducer against mumbo's fixtures, and the DSP chain on synthetic signals (an echo of a reference tone should drop by at least 20 dB after AEC, and a 1 kHz tone should come through the encode/decode round-trip).
- `podman compose -f dev/mumble-server/compose.yaml up`. Then run `cargo run -p blurt-cli -- localhost` against it next to the official Mumble client: confirm two-way audio, that UDP is chosen (the server's UserStats show UDP), and that blocking UDP falls back to TCP.
- Latency: loopback test (inject a click and measure when it comes back through the server) with the results printed by `blurt-cli --measure`.
- UI: `cargo run -p blurt` under Wayland (`WAYLAND_DISPLAY` set, no X). Check connect, channel switching, and chat. Close the window and confirm the tray icon stays and RSS falls. Fire the portal shortcuts for mute, deafen, and PTT and confirm the tray icon and server state update.
- Footprint: `/usr/bin/time -v` for RSS, `pidstat` for idle CPU, `ls -l target/release/blurt` for size.
