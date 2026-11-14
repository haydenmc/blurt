# blurt

A small, fast, native [Mumble](https://www.mumble.info/) client written in Rust.

Priorities: speed, a small resource footprint, low-latency and clean audio
(echo cancellation and noise suppression), and usability. Linux on Wayland
comes first; Windows and macOS builds are planned later.

## Install

Download `blurt.flatpak` from the [latest release](https://github.com/haydenmc/blurt/releases/latest), then:

```bash
flatpak install --user blurt.flatpak
flatpak run io.github.haydenmc.blurt
```

The release also has a plain binary tarball (built on Ubuntu 24.04; it needs
libopus and libpipewire-0.3).

## Status

| Milestone | |
| --- | --- |
| 1. Skeleton: workspace, CI, local test server | done |
| 2. Protocol: encrypted UDP voice with TCP fallback, certificates | done |
| 3. Audio: echo cancellation, RNNoise, voice gate, Opus, jitter buffer | done |
| 4. UI: servers, channels, chat, mute/deafen; settings still to come | in progress (v0.1.0) |
| 5. Tray, global hotkeys, close to tray | planned |
| 6. Polish: reconnect, notification sounds, device changes, Flathub | planned |

The full plan is in [docs/plan.md](docs/plan.md).

## Layout

| Crate | Purpose |
| --- | --- |
| `crates/mumble-protocol` | Mumble protocol and connection; forked from [Mumbo](https://github.com/citelao/mumbo) (see its `NOTICE`) |
| `crates/blurt-audio` | Real-time audio engine: capture, DSP, Opus, jitter buffering, mixing |
| `crates/blurt-platform` | System tray and global hotkeys |
| `crates/blurt` | The [iced](https://iced.rs) application |

## Development

```bash
cargo run -p blurt
cargo test --workspace
scripts/check.sh   # what CI runs: fmt, clippy with warnings as errors, tests
```

Run a local server to test against: see [`dev/mumble-server`](dev/mumble-server/README.md).

The Flatpak is built by CI ([`.github/workflows/flatpak.yml`](.github/workflows/flatpak.yml));
pushing a `v*` tag builds it and the tarball and publishes a GitHub release.
To build it yourself, outside a container (bubblewrap doesn't run in one):

```bash
scripts/flatpak-sources.sh   # crate list for the offline build; needs python3-aiohttp, python3-tomlkit
flatpak-builder --user --install-deps-from=flathub --force-clean \
  build-dir packaging/flatpak/io.github.haydenmc.blurt.yml
```

Changes to `crates/mumble-protocol` go back upstream to Mumbo; see
[docs/upstream.md](docs/upstream.md) for the commit rules and the export script.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
