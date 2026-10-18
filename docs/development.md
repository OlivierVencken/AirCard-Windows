# Development

This repository is an independently maintained continuation of Lumid-Off's
Windows AirCard port. Keep the original attribution and MIT license. Changes are
integrated here rather than prepared for an upstream merge.

## Branches and validation

Use `main` as the integration branch. Create feature or fix branches from it and
target pull requests at `OlivierVencken/AirCard-Windows:main`. Existing effect
branches retain the development history; a permanent `develop` branch is not
required. The `upstream` remote is a historical reference.

Build on Windows 10/11 x64 with stable Rust MSVC:

```powershell
cargo test --locked
cargo build --locked --release
git diff --check
```

The executable is `target/release/aircard.exe`. GitHub Actions runs tests and a
release build on pushes and pull requests to `main`, and uploads the
`aircard-windows-x64` artifact. A release build does not create a GitHub release.
Pushing a `v*` tag publishes the executable as a release; tag only when ready.

For overlay visual changes, run the explicit desktop-only screenshot check:

```powershell
$env:AIRCARD_EFFECTS_QA_DIRECTORY = Join-Path $env:TEMP 'aircard-effects-qa'
cargo test --locked wallet_overlay_native_visual_check -- --ignored --nocapture
```

Do not run all ignored tests indiscriminately. Some tests inspect local recovery
state, and ordinary tests can query Apple services when a phone is connected.
Desktop checks do not establish successful device installation or native motion.

## Effects and recovery

Read the [effects guide](wallet-effects-guide.md) before changing device work.
Preserve draft and journal compatibility, immutable original bytes, device locks,
preimage checks, complete Books restoration, and artwork-first verification
followed by two metadata-only readbacks. Never rewrite prior application folders.
Keep English and Simplified Chinese strings aligned for UI changes.

The operator confirmed all current features on an iPhone 15 running iOS 26.6.1
on 18 October 2026. Keep that evidence separate from other device/build support,
restart persistence, failure-result animation and individual presentations.
