# Building and running loopx and LoopX.app

Needs the Rust toolchain (`~/.cargo/bin` on `PATH`) and the Xcode Command Line
Tools. Full Xcode is not required.

## Build the backend

```bash
cargo build --release          # target/release/loopx
```

## Run Web mode

Use a throwaway sandbox so nothing touches the real `~/.loopx`:

```bash
export LOOP_ENGINEERING_HOME=$(mktemp -d)
export HOME=$(mktemp -d)
export LOOP_UPDATE_DIR=$(mktemp -d)
target/release/loopx serve --port 18420
```

Open `http://127.0.0.1:18420`. Without the three variables it uses the real
config and the default port 8420 (an install may have picked another port; see
the dashboard LaunchAgent plist).

## Build and run the Mac app

```bash
bin/scripts/build_loopx_app.sh target/release/loopx --arch arm64 --out dist/LoopX.app
open dist/LoopX.app
```

- `--install [DIR]` also copies the app to `/Applications` (or DIR) after quitting a running LoopX,
  so Launchpad and Spotlight find it; it does not open it.
- `--arch universal` needs a universal `loopx` (`lipo -create` the two cargo targets).
- Auto-update is on only when both `SPARKLE_FEED_URL` (https) and
  `SPARKLE_PUBLIC_ED_KEY` (base64 Ed25519 public key) are set at build time;
  setting only one is an error. See [RELEASING.md](RELEASING.md).
- The app is ad-hoc signed, so the first launch may need right-click > Open.
- The app has no Dock icon or window (`LSUIElement`): look for its icon in the menu bar,
  plus the notch or HUD once the backend is up.
- If launchd refuses to run the ad-hoc signed bundled agent (a crash report with
  `Launch Constraint Violation` in `~/Library/Logs/DiagnosticReports`), the app falls back
  to spawning the bundled `loopx serve` itself after a few seconds; that backend stops with
  the app and logs to `outputs/loopx-serve.log`. Developer ID signing removes the need for it.
- On launch the app registers the bundled `loopx` as a login-item agent with
  `SMAppService`, which changes the real machine. Use Web mode for sandboxed
  checks. If a `loopx serve` is already running (`loopx.lock`), the app connects
  to it and offers to replace it when it is older than the bundled one.

## Scheduled loops

`loopx serve` only serves the dashboard. Loops run when `loopx scheduler` polls. The
Mac app can do that for you: Settings > "Run scheduled loops while LoopX is open"
(off by default, since a due loop acts on the real GitLab and Slack) polls on launch and
every 15 minutes. Loops therefore only run while the app is open (turn on "Launch LoopX at
login" to cover that). Use one scheduler at a time: if you also load the `com.hermes.loop-engineering` launchd
agent (Web mode, `install-loopx.sh`), leave this setting off — the app no longer checks for it.

## Swift package only

```bash
cd macos/LoopX
swift build && swift test
swift run LoopX      # no bundled backend: start `loopx serve` separately
```

## Verify

```bash
cargo test && cargo clippy -- -D warnings && cargo fmt --check
LOOPX_BIN=target/debug/loopx python3 -m pytest tests/test_cli_contract_*.py tests/test_e2e_loopx_*.py -q
```
