# Auto-update plan (design only, no updater code exists)

Goal: a user can keep GitDragon current without trusting the network, the release page, or a CDN.
Nothing here is implemented; this fixes the rules so the later implementation cannot drift.

## Principles

1. **Opt-in / notify first.** Default behaviour is a "new version available" notice with a link. Silent install is a setting, off by default.
   The check is one HTTPS GET of a small manifest; no identifiers, no telemetry (the app's privacy promise stays true).
2. **Signed manifest, pinned key.** The update manifest (`latest.json`: version, per-platform URL, SHA-256, size, min-supported version)
   is signed with the project's release key (minisign/Ed25519). The **public key is compiled into the binary**; the key is not fetched
   from the same place as the manifest. Key rotation = ship a release signed by the old key that carries the new key (two keys accepted during overlap).
3. **No downgrades.** The updater only accepts a version strictly greater than the running one (semver compare) and a manifest whose
   `issued_at` is not older than the last accepted one (stored locally). This blocks replay of an old, vulnerable signed release.
4. **Verify before anything runs.** Download to a temp file in the user's data dir, check size and SHA-256 from the signed manifest, verify the
   artefact's own platform signature as well (Authenticode on Windows, notarisation on macOS), then hand over to the installer. Any failure deletes the file and keeps the current version.
5. **HTTPS only, hosts pinned by allow-list** (GitHub releases host). Redirects are followed only within that allow-list.
6. **No elevation.** The per-user Inno installer needs no admin; the updater never asks for it. Per-machine installs just show the notice.
7. **Atomic and reversible.** Windows: run the new installer silently (`/SILENT /CLOSEAPPLICATIONS`) after the app saves state and exits. macOS: replace the
   `.app` via a helper after exit. Linux package installs: notify only (package manager owns updates); AppImage: optional `AppImageUpdate`.
8. **Separate channels** (`stable`, later `beta`) use different manifest URLs and different keys if beta signing is less protected.

## Implementation sketch (later)

- New small crate `updater` (no UI): `check(current, channel) -> Option<Update>`, `download(&Update) -> Verified`, pure verification functions with unit tests
  for: bad signature, tampered hash, same/older version, stale `issued_at`, wrong host.
- The key pair is generated offline; the private key lives only in the release signer (hardware token or CI secret with approval), never in the repo.
- CI publishes `latest.json` + `latest.json.minisig` as the last step of the release, after the draft release is reviewed and published.
- Settings card: "Check for updates" (manual), "Notify on start" (default on), "Install automatically" (default off).

## Prerequisites

Signing certificates (`docs/release.md`), an app icon, and a chosen licence. Until then there is no release channel to update from.
