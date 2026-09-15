# What a Windows user must install/configure

Consolidated checklist. For the full detail behind each item, see
`docs/PACKAGING.md` (build/ship) and `docs/VIRTUAL_DEVICES.md` (why two cable
instances are needed and how the device pickers map to them).

## 1. The ClearNAI application itself

- Either `ClearNAI-Setup.exe` (recommended — Start Menu/Desktop shortcuts, a
  real "Apps & Features" entry with Uninstall) or the standalone
  `clearnairt.exe` (portable, no install). Both are produced by CI
  (`.github/workflows/windows.yml`) and documented in `docs/PACKAGING.md`.
- **No separate model/DLL files to place next to the exe.** `weya_nc.dll`
  (both the mic-path and speaker-path copies — see
  `docs/WINDOWS_LINUX_AUDIO_PARITY.md` §5 for why there are two) and the
  ONNX model bundle are embedded directly into the binary at build time and
  self-extracted into `%LOCALAPPDATA%\ClearNAI` on first run
  (`app/src/setup.rs::ensure_bvc_assets_extracted`). Nothing to download,
  nothing to configure.
- Requires Windows 10 or 11, x86_64. Checked automatically at launch
  (`setup::check_system_compatibility`) and shown on the setup screen if
  unmet — never silently ignored.

## 2. Virtual audio cable (external prerequisite — not shipped by this repo)

**Required** for other apps (Zoom/Meet/Teams/Discord) to see ClearNAI's
processed output as a selectable microphone. This project intentionally does
not implement a Windows audio driver — see
`docs/WINDOWS_LINUX_AUDIO_PARITY.md`'s architecture diagram: ClearNAI only
ever renders into / captures from an existing WASAPI endpoint.

- Install **two independent VB-Audio Virtual Cable instances** (e.g.
  VB-Audio's "VB-CABLE A+B" package), not one — one cable can't serve both
  the "virtual mic target" and "virtual speaker source" roles at once. Full
  reasoning in `docs/VIRTUAL_DEVICES.md`.
- Each is a signed third-party Windows driver install, requiring admin
  rights and possibly a reboot. This happens entirely outside ClearNAI's own
  installer/exe.
- After installing, open ClearNAI and use its "Virtual mic target" /
  "Virtual speaker source" device pickers to assign each cable's endpoint.
  If you install a cable *after* ClearNAI is already running, click
  **Refresh devices** in the GUI (devices are otherwise only scanned once at
  launch) rather than restarting the app.

Concretely, the routing a user should expect to see:

| | Endpoint |
|---|---|
| ClearNAI renders processed mic audio into | the "virtual mic target" cable's **render/playback** side |
| Other apps (Zoom, etc.) select as their microphone | that same cable's **recording** side, which Windows exposes as a normal input device |
| ClearNAI captures the "virtual speaker source" from | the second cable's **recording** side (fed by whatever the user routed into its playback side) |

Whether Zoom/Meet/Teams can actually select and use it has not been verified
from this development environment (no Windows machine, no VB-Cable install,
no real conferencing app available here) — this is a real, open item, not
claimed as done. See `STATUS.md` for the full list of what has and hasn't
been run on real hardware.

## 3. Runtime dependencies

- No .NET, no Python, no separate ML runtime to install — `nnnoiseless` and
  `deep_filter`/`tract` (the noise engines), `wasapi` (WASAPI bindings), and
  `iced`/`wgpu` (GUI) are all statically linked into `clearnairt.exe` by
  `cargo build --release`. `docs/PACKAGING.md`'s Option B calls out running
  `dumpbin /dependents` on the real MSVC-built exe to confirm no unexpected
  non-system DLL dependency was introduced — this has not yet been run
  against a real MSVC build (see `STATUS.md`, item 1).
- Standard VC++ runtime as normally present on Windows 10/11; nothing
  project-specific pinned beyond what MSVC/`cargo build` itself requires.

## 4. Permissions

- Standard Windows microphone privacy permission (Settings → Privacy →
  Microphone) must allow desktop apps to access the microphone, same as any
  other voice application. Not handled specially by this app; if denied, mic
  capture will fail to open and the failure is now retried/logged rather
  than silently killing the capture thread forever (see the WASAPI
  reconnect-loop fix in `crates/audio-io`).
- No admin rights needed to install/run ClearNAI itself (the NSIS installer
  is a per-user install). Admin rights ARE needed for the separate VB-Cable
  driver installs (§2), which is normal for any Windows audio driver.

## 5. Audio device configuration

- ClearNAI does not assume a fixed device sample rate — WASAPI negotiates
  whatever the physical device's shared-mode format is;
  `warn_if_buffer_size_mismatched` logs (not fails) if the negotiated buffer
  size isn't the requested 480-sample/10ms period, since real hardware has
  been observed to round this to a different value. The fixed 48kHz/mono/f32
  contract described in `docs/WINDOWS_LINUX_AUDIO_PARITY.md` is enforced at
  the WASAPI adapter boundary (`engine_wave_format()`), not assumed to be
  what the hardware natively provides.
- If the physical mic or a virtual cable device is disabled/removed/renamed
  while ClearNAI is running, the affected WASAPI worker thread now retries
  with backoff instead of dying permanently (see `crates/audio-io`'s
  reconnect-loop fix) — no restart needed once the device comes back.
