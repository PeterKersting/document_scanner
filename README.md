# Document Scanner
Wrote this as a fast and lean alternative to Mac native document scanner app, with better UX and missing features.

## Build the app

Install the Xcode Command Line Tools and a current stable Rust toolchain:

```sh
xcode-select --install
```

Install Rust and Cargo using the instructions at [rustup.rs](https://rustup.rs/).
Then, from the repository root, build and run the app:

```sh
cargo run --release
```

To build a macOS application bundle instead, run:

```sh
sh bundle.sh
open target/release/ScanDesk.app
```

The bundle is created at `target/release/ScanDesk.app`. The bundling script
replaces any existing bundle at that path. macOS may ask for local network
access so the app can discover network scanners.
