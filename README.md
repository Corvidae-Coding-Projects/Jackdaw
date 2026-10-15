# Jackdaw

Jackdaw is an experimental browser written in Rust for terminal-driven and agent-driven web workflows. Its current executables retain their established names: `browserd` is the browser service, `rwb` is the command-line client, and `rwb-gui` is an experimental software-rendered Linux frontend.

The project is building toward four end-to-end terminal workflows: reading and following links, logging in to interactive applications, filling and submitting forms, and uploading and downloading files. HTTPS, persistent profiles, semantic observations, form interaction, CPU-rendered frames, and native transfers have working foundations. Jackdaw is not yet a general-purpose replacement for a mature browser: ordinary-site compatibility, production tab-process isolation, broad DOM and scheduler behavior, frames and popups, origin policy, JavaScript transfer objects, crash recovery, and final terminal-MVP qualification remain active work.

## Rust components

Jackdaw integrates Rust-native components rather than embedding a foreign browser engine:

- Blitz for HTML, style, and layout foundations
- Boa for ECMAScript execution
- Vello CPU for software rasterization
- redb for profile and operation persistence
- rustls with the OxiTLS RustCrypto provider and root store for TLS
- Parley and an embedded Cantarell font for text

Exact locked versions and transitive dependencies are recorded in `Cargo.lock`. Third-party software and assets retain their own licenses; see `THIRD_PARTY_NOTICES.md`. In particular, the embedded Cantarell font is OFL-1.1 licensed and is not covered by Jackdaw's MIT license. The certificates and private keys under `crates/browser-net/fixtures/tls/` are synthetic localhost test fixtures, not production credentials.

## Linux prerequisites

The repository pins Rust 1.96.0 with rustfmt and Clippy in `rust-toolchain.toml`. Install rustup, a C compiler/linker, `pkg-config`, Python 3, and the ordinary Linux development packages needed by the selected GUI/X11 crates. Terminal use is designed to work without `DISPLAY`, `WAYLAND_DISPLAY`, a GPU, or a system TLS library. The GUI requires a usable Linux desktop/display environment.

On a rustup-managed system, entering the repository selects the pinned toolchain automatically.

## Build

Build the locked release workspace from the repository root:

```sh
cargo build --release --locked --workspace --bins
```

The primary executables are then available at `target/release/browserd`, `target/release/rwb`, and `target/release/rwb-gui`.

## Start the daemon and use the CLI

Choose private runtime and profile directories. The daemon creates a user-only socket and owns browser/profile state:

```sh
mkdir -p "$XDG_RUNTIME_DIR/jackdaw" "$HOME/.local/share/jackdaw/profiles"
./target/release/browserd \
  --socket "$XDG_RUNTIME_DIR/jackdaw/browser.sock" \
  --profile-root "$HOME/.local/share/jackdaw/profiles"
```

In another terminal, create and open a profile, then create a tab. The commands return versioned JSON containing protocol identifiers. In a fresh profile root, `profile create default` returns profile ID `p_default`, and the first tab creation returns tab ID `t_1`; use the IDs from your own JSON responses if they differ:

```sh
SOCKET="$XDG_RUNTIME_DIR/jackdaw/browser.sock"
./target/release/rwb --socket "$SOCKET" --json profile create default
./target/release/rwb --socket "$SOCKET" --json profile open p_default
./target/release/rwb --socket "$SOCKET" --json --profile p_default tab new https://example.com/
./target/release/rwb --socket "$SOCKET" --json --profile p_default tab list
./target/release/rwb --socket "$SOCKET" --json --profile p_default --tab t_1 observe --mode all
./target/release/rwb --socket "$SOCKET" --json --profile p_default --tab t_1 read
```

Follow a discovered link or operate a control using the generation-bound element reference returned by `observe`:

```sh
./target/release/rwb --socket "$SOCKET" --profile p_default --tab t_1 click ELEMENT_REF
./target/release/rwb --socket "$SOCKET" --profile p_default --tab t_1 fill ELEMENT_REF "value"
./target/release/rwb --socket "$SOCKET" --profile p_default --tab t_1 submit ELEMENT_REF
```

Upload and download commands use explicit operator paths:

```sh
./target/release/rwb --socket "$SOCKET" --profile p_default --tab t_1 upload ELEMENT_REF --file ./input.bin
./target/release/rwb --socket "$SOCKET" --profile p_default --tab t_1 download start https://example.com/file.bin
./target/release/rwb --socket "$SOCKET" --profile p_default --tab t_1 download wait --id DOWNLOAD_ID --out ./file.bin
```

Stop the daemon cleanly when finished:

```sh
./target/release/rwb --socket "$SOCKET" daemon stop
```

Alternatively, `rwb` can launch the adjacent release daemon and wait for readiness:

```sh
./target/release/rwb \
  --socket "$XDG_RUNTIME_DIR/jackdaw/browser.sock" \
  daemon start \
  --profile-root "$HOME/.local/share/jackdaw/profiles" \
  --browserd ./target/release/browserd
```

Run either executable without sufficient arguments to print its current usage contract. `browserd serve --stdio --profile-root PATH` exposes the same protocol as newline-delimited JSON over standard input/output.

## Experimental GUI

With `browserd` running, `rwb-gui` can present daemon-owned frames and send input through the shared protocol. It is an experimental later milestone; CLI terminal completion and shared-core behavior are the current focus. Do not interpret the existence of the GUI or a successful page load as a claim of ordinary-web compatibility.

## License

Jackdaw's own source code is available under the MIT License. See `LICENSE` and `THIRD_PARTY_NOTICES.md`.
