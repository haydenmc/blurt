# Sending protocol changes upstream to Mumbo

`crates/mumble-protocol` is a fork of the crate of the same name in
[Mumbo](https://github.com/citelao/mumbo), at the same path in both repos.
Changes to it are meant to go back upstream as PRs, so the fork is kept in a
shape where every commit to it turns into a patch Mumbo can apply directly.

## Rules for commits

1. **The crate stays identical to upstream, plus our upstreamable commits.**
   Don't make blurt-only edits to its files (not even its `Cargo.toml`
   metadata). The one exception is `NOTICE`, which the export leaves out.
2. **Commits to the crate touch only the crate** (and the root `Cargo.lock`,
   which the export leaves out; Mumbo regenerates its own). Changes to blurt's
   own crates go in separate commits, even when they're for the same feature.
3. **One topic per commit.** Each commit becomes a patch, and a run of patches
   becomes a PR, so write the message for a Mumbo reviewer.
4. **A crate change that must not go upstream** gets an `Upstream: skip`
   trailer in its commit message. This should be rare: prefer making it an
   option upstream would accept too.
5. **When syncing from upstream** (taking new Mumbo commits into the crate),
   make the sync its own commit with an `Upstream-Sync: <mumbo commit hash>`
   trailer, and update `NOTICE`. The export starts from the newest such commit.

## Exporting patches

```bash
scripts/upstream-patches.sh                 # writes target/upstream-patches/*.patch
scripts/upstream-patches.sh --verify        # also applies them to a fresh Mumbo
                                            # clone and runs the crate's tests
scripts/upstream-patches.sh --verify DIR    # write the patches somewhere else
```

To open PRs, apply them in a Mumbo checkout on one branch per PR:

```bash
git switch -c udp-voice main
git am /path/to/blurt/target/upstream-patches/000{1,3}-*.patch
cargo build   # updates Cargo.lock for any new dependencies
```

`--verify` only builds and tests the crate. Mumbo's app (`src-tauri`) and its
TypeScript protocol schema (`src/protocol.ts`) may need matching changes;
see below.

## Changes Mumbo will need alongside the current patches

These patches change parts of the crate's API that Mumbo's app uses:

- **`ConnectionOptions` has new fields** (`pinned_certificate`, `identity`,
  `legacy_voice`, `access_tokens`, `client_release`) and implements
  `Default`. Mumbo's two struct literals in `src-tauri/src/lib.rs` need
  `..Default::default()`, and it should pass
  `client_release: Some("mumbo 0.1.0".into())` to keep identifying as Mumbo.
- **`Connection` has a new `server_certificate` field.** The `let Connection
  { client, debug, audio, mut events } = …` destructuring needs `..`.
- **Server certificates are now verified.** Self-signed servers (most of
  them) fail with `MumbleConnectionError::UntrustedCertificate { fingerprint,
  .. }` until the app pins the fingerprint via
  `ConnectionOptions::pinned_certificate`. Mumbo needs a "trust this
  certificate?" prompt, and should store the pin per server.
- **The shared JSON fixtures changed.** `ServerToClientMessage` has a new
  `Unknown { type_id, payload }` variant and `User` a new `user_id` field.
  `src/protocol.ts` (whose tests parse the same fixtures) needs both.

Update this list as patches are added or merged.
