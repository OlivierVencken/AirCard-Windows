# Wallet effects and recovery

All current features, including parallax, cross-dissolve, particles and transaction
effects, were operator-confirmed working on the tested iPhone 15 running
iOS 26.6.1 on 18 October 2026. Other devices/builds, every presentation, failure
results and persistence through another restart have not been established by
that confirmation. Particles and transaction effects keep their experimental labels.

## Apply an overlay

1. Connect one unlocked, trusted iPhone by USB. Overlay operations require a
   USB-only session; WiFi-only and mixed USB/WiFi sessions are rejected.
2. In **Wallet**, select or scan the card, then use **Choose Image...** for its
   background (PNG, JPG or WebP).
3. Enable **Overlay** and choose a transparent PNG with **Choose Overlay...**.
   Drag the preview to adjust the shared crop. The preview is static.
4. Optionally enable effects:
   - **Cross-dissolve:** choose an opaque alternate background. The alternate
     shows at rest; tilt reveals the base while parallax continues.
   - **Particles (experimental):** choose a visible transparent PNG sprite.
     The native preset uses twinkling, low density and physics off.
   - **Transaction effect (experimental):** choose a transparent emitter PNG
     and a single-path SVG. Native animation was confirmed after a completed
     Apple Pay payment. Opening or tilting the card alone is not a confirmed trigger.
5. Click **Apply Card Skin** and wait for capture, original-byte backup,
   installation and verification. Invalid or stale artwork blocks Apply.
6. Force-close and reopen Wallet and Apple Pay.

Effects can be combined. The base composite supplies the static fallback.
Use background margins and keep the foreground subject clear of crop edges.
An opaque checkerboard is not transparency. Transaction SVGs need positive pixel
width/height and one direct path child with only a `d` attribute; flatten groups
and transforms first. An optional viewBox must match `0 0 width height`.

## Backups and Restore previous skin

Normal launches require recoverable originals for complete metadata and every
overwritten resource. A missing protected export means Unknown, not absent.
Recovered dynamic PDFs block Apply because they can take precedence over PNGs.
Byte readback confirms delivery; visible native motion is separate evidence.

Keep `%LOCALAPPDATA%/AirCard/effects-applications/` and the current executable.
Application folders contain frozen inputs, packs, immutable original bytes and
recovery journals. Never delete or rewrite earlier application folders.

Overlay **Restore previous skin** restores the state before the latest overlay
application, which may already contain effects. Recovery remains available after
an app restart or failure. This differs from ordinary **Restore Original**, which
uses the original card-face backup. Reopen Wallet and Apple Pay after restoration.
Use Restore before removing an installed effect from a configuration; arbitrary
changes or unsupported metadata are rejected.

## Explicit trial launchers

Build with `cargo build --locked --release`, close AirCard, then use a launcher
only when accepting its incomplete recovery limit:

| Launcher | Originals allowed to remain Unknown |
| --- | --- |
| `try-particles-experimental.cmd` | The three neutralEmitter PNG scale candidates |
| `try-transaction-experimental.cmd` | The three transaction emitter PNGs and shape SVG |

Each launcher sets its exception for that process only, clears the other trial
flags and opens `target/release/aircard.exe` without applying automatically.
Direct executable launches remain strict. These exceptions do not waive metadata,
base artwork or alternate background originals.

The journal records the policy before installation. Restore honors it after an
app restart, reapplies known originals, reports incomplete recovery and retains
the recovery selection. Unknown-original files may remain on the phone; general
protected-file deletion is not proven. Conflicting held bytes stop cleanup.
Matching candidate readbacks never become fabricated originals. Retain the
executable that understands the journal for recovery.
