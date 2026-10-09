# winget manifest templates

Templates for `microsoft/winget-pkgs` (manifest schema 1.6, multi-file layout). Nothing here is submitted.
Placeholders in `<ANGLE_BRACKETS>` are filled in at release time. `InstallerType: inno` matches the Inno Setup installer.

1. Build and publish the installer to a GitHub release (unsigned installers trigger SmartScreen warnings).
2. Fill the placeholders (`<VERSION>`, `<SHA256_OF_SETUP_EXE>` from SHA256SUMS, release date).
3. Validate: `winget validate --manifest <dir>`, then test: `winget install --manifest <dir>` (needs `winget settings --enable LocalManifestFiles`).
4. Submit with `wingetcreate` or a PR to `microsoft/winget-pkgs` under `manifests/i/InfernoDragon0/GitDragon/<VERSION>/`.

The files must be renamed/placed as three files in one folder; winget requires the names to match the package identifier.
