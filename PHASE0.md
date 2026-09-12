# Phase 0 — Research & Decisions

**Status: research verified against live sources on 2026-09-11. Decisions below are made by me (acting as architect on this port); flagged for your override.**

## Environment constraint (read this first)

This port is being developed on a **Linux** machine with no Windows box, no audio
hardware to exercise WASAPI, and no ability to install/test-sign a driver. Per
agreement with the user: I write the full source tree as carefully as I can,
label everything honestly as "not run on real Windows hardware" until you build
and test it on an actual Windows machine, and each phase report will say
explicitly which claims are "compiles" vs "verified working."

## Findings that corrected the original brief

1. **`VirtualDrivers/Virtual-Audio-Driver` is kernel-mode** (WDK, based on
   Microsoft's Sysvad sample) — not user-mode/UMDF as originally assumed.
2. Its "attempts to implement full microphone routing" release (25.5.3) and
   its README document **no mic-injection mechanism whatsoever** in the public
   version — not "recently added," genuinely undocumented. The paid custom-build
   pitch (named pipes / shared memory) is the only place any injection mechanism
   is even named.
3. **Hush (BVC) does ship a Windows build**: `pulp-vision/Hush` publishes
   prebuilt `weya_nc.dll` with a documented C API (`weya_nc.h`, ~10 functions),
   contrary to the brief's assumption that only a Linux `.so` exists. BVC is
   very likely portable via FFI, same shape as the existing RNNoise wrapper.
4. **`webrtc-audio-processing`'s `bundled` feature needs `autotools`,
   `libtoolize`, `pkg-config`, `automake`** to build its vendored C++ — a
   Unix-toolchain shape. This is a real, unresolved risk for an MSVC target;
   likely needs MSYS2/mingw scaffolding. Not yet confirmed to build under MSVC.
5. `deep_filter` (tract-based, streaming) and `iced`'s built-in `toggler`
   widget are both confirmed real. `floe-ui` (cited in the brief as a themed
   toggle library) did **not** turn up in search — treating it as unconfirmed;
   styling the toggle via `iced`'s own `Style`/`Catalog` API instead.

## Decisions

- **Virtual device mechanism: VB-Audio Virtual Cable (Plan B in the brief),
  chosen as primary — user's explicit choice**, over adapting
  `Virtual-Audio-Driver`. Reasoning: the open driver's mic-injection path is
  undocumented, not merely unproven, and this port's whole workflow
  (I write blind, you test on real hardware) makes debugging an undocumented
  kernel driver's internals the worst possible task to hand off remotely.
  VB-Cable is mature and works today; the engine talks to its "CABLE
  Input"/"CABLE Output" endpoints via ordinary WASAPI calls, same as any real
  device. Cost: branding is "CABLE Input/Output" in other apps' device
  pickers, not "ClearNAI Microphone/Speaker". A rename/relabel via driver
  adaptation is left as a documented stretch goal, not built now.
- **Audio I/O crate: `wasapi`** (not `cpal`) — confirmed to expose loopback,
  exclusive/shared mode, and event/polled buffering directly, matching the
  low-level control this project's Linux history required.
- **AEC**: attempt `webrtc-audio-processing` `bundled` first; because its
  build toolchain requirement is a real open risk, Phase 5 will explicitly
  test this before assuming it and will fall back to a pure-Rust NLMS crate
  (`fdaf-aec`) if it doesn't build under MSVC, per the brief's own fallback
  instruction.
- **GUI: `iced`**, built-in `toggler` widget, custom `Style`/`Catalog` theming
  (not `floe-ui`, unconfirmed to exist).
- **BVC**: FFI wrapper against `weya_nc.dll` / `weya_nc.h`, same pattern as
  the existing RNNoise C FFI wrapper. Available on Windows — brief's assumption
  it might not be is corrected.

## Open risks carried into later phases

- AEC bundled C++ build under MSVC: unresolved, real risk, test in Phase 5.
- VB-Cable is a runtime dependency the user must install separately; not
  redistributable inside our installer (closed-source, third-party EULA).
- Nothing in this project has been run. All of the above is source/docs
  research, not execution.
