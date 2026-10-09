# Releasing GitDragon

Status (2026-10-09): everything here is **local-only**. The workflows in `.github/workflows/` have never run, and
builds are **unsigned**. Signing hooks exist but are disabled; this page says what turns each one on.

## Cutting a release

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), refresh `Cargo.lock` (`cargo update -w`), commit.
2. `cargo deny check` and `cargo audit` are clean; `cargo test --workspace` passes.
3. Tag `vX.Y.Z` and push the tag. `release.yml` checks that the tag equals the Cargo version, builds with `--locked`
   on Windows, macOS and Linux, packages, writes `SHA256SUMS`, attaches a build-provenance attestation, and creates a
   **draft** GitHub release. Review the draft, then publish by hand.
4. Fill in and submit the winget manifest (`packaging/windows/winget/README.md`).

## Local Windows build

```powershell
cargo build --release --locked -p gitdragon
powershell -File packaging/windows/build.ps1        # writes dist/: portable zip, symbols zip, SHA256SUMS-windows.txt
```

The same script also builds the installer when Inno Setup 6 is installed (`winget install JRSoftware.InnoSetup`, once).
Without it the script still produces the portable zip and prints a hint. Output goes to `dist/` (untracked).

## What ships

| Platform | Artefacts | Packaging |
|---|---|---|
| Windows x64 | `GitDragon-<v>-windows-x64-setup.exe` (per-user), `gitdragon-<v>-windows-x64-portable.zip`, `...-symbols.zip` (PDB) | `packaging/windows/` |
| macOS arm64 | `GitDragon-<v>-macos-arm64.dmg` | `packaging/macos/build-dmg.sh` |
| Linux x64 | `.AppImage`, `.deb`, `.tar.gz`, `.debug` | `packaging/linux/` |

Release profile: thin LTO, one codegen unit, `debug = "line-tables-only"`. On Windows the debug info lives in a separate
PDB, so the exe stays small; on Linux CI splits it with `objcopy --only-keep-debug`. Panic strategy is unchanged (unwind),
because abort would save little size and unwinding lets a panic in a worker thread be contained instead of killing the app. Windows links the CRT statically (`.cargo/config.toml`).
Do not set `RUSTFLAGS` in CI: the variable replaces the config file's rustflags and silently drops `+crt-static`.

## Signing (not set up yet)

Unsigned builds work but trigger SmartScreen (Windows) and Gatekeeper (macOS) warnings. Steps to enable each, later:

### Windows (Authenticode)
1. Get a code-signing certificate (EV gives instant SmartScreen reputation; OV builds it over time) or use Azure Trusted Signing.
2. Store the cert in the runner (or a cloud HSM) and its thumbprint in a repository secret, e.g. `WINDOWS_CERT_THUMBPRINT`.
3. In `release.yml`, uncomment the `GITDRAGON_SIGN` / `GITDRAGON_SIGN_THUMBPRINT` env on the Windows packaging step.
   `packaging/windows/build.ps1` then signs `gitdragon.exe` before zipping and the installer after it is built.
4. In `gitdragon.iss`, enable `SignTool` / `SignedUninstaller` and pass `/Ssigntool=...` to `iscc`.
5. Always timestamp (`/tr` + `/td sha256`) so signatures outlive the certificate.

### macOS (Developer ID + notarisation)
1. Apple Developer Program membership; create a "Developer ID Application" certificate.
2. In CI import the certificate into a temporary keychain from secrets; create a `notarytool` keychain profile (App Store Connect API key).
3. Set `APPLE_SIGN_IDENTITY` and `APPLE_NOTARY_PROFILE` on the macOS packaging step. `build-dmg.sh` then signs with the hardened
   runtime, notarises the dmg, and staples the ticket. Without them it ad-hoc signs (required to run on Apple Silicon).
4. Add entitlements only if something needs them (JIT is not used; network client is allowed by default for non-sandboxed apps).

### Checksums and downloads
1. `SHA256SUMS` is generated in the publish job. Sign it with a project key (minisign or cosign) so a compromised release page
   alone cannot swap files; commit the public key to the repo and embed it in the updater (see `docs/auto-update.md`).
2. Build-provenance attestation (`actions/attest-build-provenance`) is already wired: `gh attestation verify <file> -R InfernoDragon0/GitDragon`.

### Linux
Optional: GPG-sign the `.deb` / an apt repository, and sign the AppImage (`appimagetool --sign`).

## Supply-chain rules for CI

- Every action is pinned to a full commit SHA with the tag in a comment; bump them deliberately (re-verify with `git ls-remote --tags`).
- Workflows default to `permissions: contents: read`; only the publish job gets `contents: write`, `id-token`, `attestations`.
- `--locked` on every cargo invocation; `cargo deny` (licences, bans, sources, advisories) and `cargo audit` gate CI.
- `persist-credentials: false` on checkout. No third-party code runs with secrets except the release action in the publish job.
- Open items: pin Inno Setup (Chocolatey) and `appimagetool` by version + checksum in the release workflow.

## Gaps before the first public release

- App icon: no `.ico` / `.icns` / `.png` exists yet (installer, exe resource, dmg, AppImage all have `TODO(icon)` markers).
- Project licence undecided (installer and winget manifest carry `TODO(license)`).
- Signing certificates (above).
