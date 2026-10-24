# Releasing loopx

One tag produces everything: the signed and notarized `loopx` binary, `LoopX.app`
and its DMG, the Sparkle appcast, `SHA256SUMS`, the GitHub Release (notes from
`CHANGELOG.md`) and the Homebrew formula in the tap. The pipeline is
`.github/workflows/release.yml`; every signing step is skipped with a log line
when its secret is missing, so a first dry run on a fork still yields unsigned
artifacts.

## What a release contains

| Asset | Used by |
| --- | --- |
| `loopx-darwin-arm64.tar.gz`, `loopx-darwin-x86_64.tar.gz`, `loopx-darwin-universal.tar.gz` (each holds one file, `loopx`) | `bin/scripts/install-loopx.sh` (native arch first, universal as fallback), the Homebrew formula (universal) |
| `loopx-support.tar.gz` (`config/*.template`, `launchd/loopx/*.plist.template`) | `install-loopx.sh` (config skeleton, launchd plists), the formula (`pkgshare`) |
| `LoopX-X.Y.Z.dmg`, `LoopX-X.Y.Z.app.zip` | people installing the Mac app; Sparkle downloads the DMG |
| `appcast.xml` | Sparkle (`SUFeedURL` = `https://github.com/encoreshao/loop-engineering/releases/latest/download/appcast.xml`) |
| `SHA256SUMS` | `install-loopx.sh` verifies every download against it before installing |

## One-time setup (maintainer)

1. **Developer ID Application certificate.** Apple Developer account, Certificates,
   "Developer ID Application". Export it from Keychain Access as a `.p12` with a password.
   Secrets: `APPLE_CERT_P12_BASE64` (`base64 -i cert.p12 | pbcopy`), `APPLE_CERT_PASSWORD`,
   `APPLE_SIGNING_IDENTITY` (the exact name, e.g. `Developer ID Application: Name (TEAMID)`;
   see `security find-identity -v -p codesigning`).
2. **notarytool credentials.** App Store Connect, Users and Access, Integrations, create a
   Team API key (Developer role is enough). Secrets: `APPLE_API_KEY_P8_BASE64` (`base64 -i AuthKey_XXXX.p8`),
   `APPLE_API_KEY_ID`, `APPLE_API_ISSUER`. Check locally first:
   `xcrun notarytool history --key AuthKey_XXXX.p8 --key-id XXXX --issuer <uuid>`.
3. **Sparkle EdDSA key.** From a Sparkle release (`Sparkle-2.x.tar.xz`): `./bin/generate_keys`
   stores the private key in your login Keychain and prints the public key. Put the public key in the
   app's `Info.plist` as `SUPublicEDKey` (and the feed URL as `SUFeedURL`, see above): `build_loopx_app.sh` writes both from the `SPARKLE_PUBLIC_ED_KEY` and `SPARKLE_FEED_URL` environment variables and the app starts Sparkle only when both are set. Export the
   private key with `./bin/generate_keys -x sparkle_private.key` and save it as the
   secret `SPARKLE_ED_PRIVATE_KEY` (the file's contents). Losing this key means existing installs can
   never update again: back it up.
4. **Tap.** Create the repository `encoreshao/homebrew-tap` (it can be empty). Create a fine-grained
   personal access token with Contents: read and write on that repo only, and save it as
   `TAP_GITHUB_TOKEN`. To use another tap, set the repository variable `TAP_REPO`.
5. **The app build script.** The workflow runs `bin/scripts/build_loopx_app.sh <universal loopx> --out dist/app/LoopX.app`
   (ad-hoc signed), then re-signs the embedded `loopx` and the bundle with the Developer ID identity (hardened
   runtime) when `APPLE_SIGNING_IDENTITY` is available. Sparkle's helper binaries, once the framework is added, must
   be signed first, inside-out.

## Cutting a release

```bash
# 1. changelog: move Unreleased to the new version (CLAUDE.md, "Changelog and releases")
#    - rename "## [Unreleased]" to "## [X.Y.Z] - YYYY-MM-DD", add a fresh empty "## [Unreleased]" above
#    - update the link references at the bottom
# 2. bump the workspace version in Cargo.toml ([workspace.package] version = "X.Y.Z"), then refresh the lockfile
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release -p loopx            # updates Cargo.lock
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
LOOPX_BIN=target/debug/loopx python3 -m pytest tests/ -q
bin/scripts/release_notes.sh X.Y.Z        # preview the GitHub Release body
# 3. commit, tag, push (the tag must be vX.Y.Z and equal the Cargo.toml version, or the workflow stops)
git commit -am "Release X.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin HEAD vX.Y.Z
```

Then watch the "Release" workflow. When it is green:

```bash
gh release view vX.Y.Z                                    # assets and notes
curl -fsSL https://github.com/encoreshao/loop-engineering/releases/download/vX.Y.Z/SHA256SUMS
brew update && brew install encoreshao/tap/loopx && loopx --version
```

A tag with a `-` in it (`v0.5.0-rc1`) is published as a pre-release; `install-loopx.sh` without
`--version` and Sparkle only follow the latest full release. To rebuild an existing tag
(for example after adding a missing secret), run the workflow manually (Actions, Release,
Run workflow) with that tag: assets are replaced with `--clobber`.

Without CI, the same steps by hand: `bin/scripts/build_loopx_app.sh`, `codesign --options runtime --timestamp`,
`xcrun notarytool submit --wait`, `xcrun stapler staple`, `shasum -a 256 * > SHA256SUMS`,
`gh release create`, `packaging/homebrew/render_formula.sh X.Y.Z <sha universal> <sha support> > Formula/loopx.rb`.

## Installing and upgrading from a release

```bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash -s -- --upgrade
bin/scripts/uninstall-loopx.sh --project-dir ~/.loopx   # --keep-config, --remove-app
```

`--upgrade` keeps your config and replaces only the program; see the header of `install-loopx.sh`.
Web mode to app: `LoopX.app` finds the running backend through `~/.loopx/loopx.lock` and adopts it
(`loopx doctor` prints whether one is running), so the Web install can stay in place.

## What could not be verified here

These need real credentials or a clean Mac and have only been checked structurally (workflow YAML shape, shell
and Ruby syntax, local fixtures), never run:

- Developer ID signing, hardened-runtime launch, notarization and stapling of the binary, the app and the DMG.
- `build_loopx_app.sh` being invoked as above, Sparkle `generate_appcast` flags for the pinned Sparkle version
  (2.6.4) and an actual in-app update from one release to the next.
- The tap push and `brew install` / `brew services start loopx` / `brew test loopx` on a clean Mac; the formula
  only has the `serve` job as a service (Homebrew allows one), the scheduler job is set up from the caveats.
- `install-loopx.sh` against a real GitHub Release (it is tested against local `file://` and HTTP fixtures),
  and its launchd steps against the real launchd (tested with a fake `launchctl`).
- `uninstall-loopx.sh` against the real Keychain (tested with a fake `security`) and the app bundle's
  `SMAppService` login item (remove it in System Settings, Login Items, if it lingers).
- The acceptance run itself: install by app and by Homebrew on a clean machine, finish configuration, run one loop,
  and upgrade from Web mode to the app with the backend adopted.
