# blurt

A small, fast, native [Mumble](https://www.mumble.info/) client written in Rust.

Priorities: speed, a small resource footprint, low-latency and clean audio
(echo cancellation and noise suppression), and usability. Linux on Wayland
comes first; Windows and macOS builds are planned later.

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

Changes to `crates/mumble-protocol` go back upstream to Mumbo; see
[docs/upstream.md](docs/upstream.md) for the commit rules and the export script.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
