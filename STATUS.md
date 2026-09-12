# ClearNAI for Windows — Status Report (2026-09-11)

Full source tree built across all 7 phases. **Everything below is honest
about the central constraint: this was developed on a Linux machine with no
Windows hardware, no MSVC toolchain, and no audio devices to test against.**
Per agreement with the user, the deliverable at this point is a complete,
carefully-verified-as-far-as-possible source tree; a Windows machine is
needed to close the loop on the claims marked unverified below.

## The six required answers, kept separate as instructed

1. **Does it build cleanly?**
   Yes, in a stronger sense than "typechecks": the full workspace was cross-
   checked against `x86_64-pc-windows-gnu` (`cargo check --workspace`, clean,
   zero warnings, from-scratch rebuild) **and actually built and linked** into
   a real 627MB PE32+ Windows executable (`file` confirms
   `PE32+ executable for MS Windows ... x86-64`) using a mingw-w64 GNU
   toolchain installed without root access in this sandbox. That is real,
   run evidence, not a guess.
   **However**, the project's actual target/deliverable is
   `x86_64-pc-windows-msvc`, which cannot be built or linked from this Linux
   machine at all (no MSVC linker exists here). A clean GNU build is strong
   positive signal (same source, same crate graph, same COM/WASAPI bindings)
   but is explicitly **not** proof MSVC will succeed — different linker,
   different import libraries, and `tract-linalg` hand-compiles x86_64 SIMD
   assembly via the `cc` crate, which behaves differently under `cl.exe`
   than under GCC. **The MSVC build itself has never been attempted.**
   66 unit tests pass natively on Linux across all 7 library crates.

2. **Does the virtual mic actually appear/work in another real app?**
   **Not verified — cannot be, from this machine.** The code path exists
   (mic capture → AEC → Noise → BVC → Studio → render into a user-selected
   virtual-mic-target device) and type-checks/links against the real
   `wasapi` crate API, but no Windows box, no VB-Cable install, and no real
   app (Zoom/Discord/etc.) were available to actually select and record from
   it.

3. **Does the virtual speaker actually appear/work?**
   **Not verified**, same reason. Code path exists (capture from a user-
   selected virtual-speaker-source device → Noise+BVC → render to real
   hardware), type-checks/links, never run.

4. **Does the speaker-inbound tap capture real, unaltered audio?**
   **Not verified.** `audio-io::loopback` implements the render-device-
   opened-for-capture WASAPI trick (confirmed from the crate's own source,
   not guessed) and ships `record_loopback_to_wav()` specifically so this
   claim can be tested on real hardware — that test has not been run here.

5. **Does AEC measurably reduce echo in a real speaker-to-mic test?**
   **Not verified against real acoustics.** What IS verified: `webrtc-audio-
   processing`'s `bundled` AEC3 build was actually attempted on this Linux
   box and **failed** (bindgen couldn't find clang's resource headers, even
   with meson/ninja manually installed) — a real, captured failure, not an
   assumption. The fallback, a hand-written pure-Rust NLMS adaptive filter,
   was built and **did** pass a real, meaningful synthetic test: ~120x
   (20.8dB) echo-energy reduction after 8 seconds of convergence against an
   algebraically-known synthetic echo signal. That is a genuine DSP result,
   but it is a synthetic single-path stationary echo model, not room
   acoustics, not a real speaker-to-mic test. AEC defaults OFF per spec.

6. **Does every GUI toggle enable/disable its stage live, without dropout?**
   **Not verified — the GUI has never been run or seen.** Architecturally,
   every stage is constructed at startup regardless of toggle state, and
   toggles flip a shared `StageToggle`/`AtomicBool` that the processing loop
   reads every 10ms frame (`dsp_core::run_if_enabled`) — no teardown/rebuild
   path exists in the code for a toggle flip, which is the correct pattern
   for the "no glitch on toggle" requirement, but whether it actually behaves
   glitch-free on real audio hardware timing has not been observed.

## What corrected the brief along the way (real findings, not assumptions)

- `Virtual-Audio-Driver` is kernel-mode, not user-mode, and has an
  **undocumented** mic-injection mechanism even in its "full mic routing"
  release — led to choosing VB-Audio Virtual Cable as the primary approach
  (your explicit decision), which then surfaced a real, previously-unstated
  architecture problem: **one VB-Cable install can't serve both the virtual
  mic and virtual speaker roles at once** — needs two independent cable
  instances; documented in `docs/VIRTUAL_DEVICES.md` with a GUI device-picker
  fallback rather than a hardcoded guess.
- Hush/BVC does ship a real Windows DLL (`weya_nc.dll`) with a documented C
  API — confirmed by fetching the real header, not by trusting the brief.
- RNNoise was implemented via the pure-Rust `nnnoiseless` crate instead of
  vendoring Xiph's C library, specifically to avoid an MSVC/cc-crate C
  cross-compilation fight — a deliberate, documented deviation.
- A real prior reference implementation of this exact project was found on
  this machine at `/home/lemon-peak/krsipCl` partway through the build. An
  audit against it caught genuinely wrong, invented values in the first pass
  of `studio-dsp` (wrong filter frequencies, wrong compressor ratios, wrong
  de-esser architecture, Natural preset's de-esser on when it should be off)
  and a wrong BVC attenuation default (100.0dB guessed vs. the real 30.0dB)
  — both corrected against the real source before this report.
- `wasapi::Device` was discovered, via a real compiler error (not docs), to
  not be `Send` — worked around by moving device-id strings across threads
  instead.

## Known, honestly-stated gaps

- MMCSS/realtime thread-priority: the `wasapi` crate exposes no wrapper for
  it; audio threads run at normal OS priority. Not faked, not solved.
- BVC's `FrameAdapter` tolerates any DLL frame-length mismatch rather than
  failing fast like the reference — a deliberate, documented divergence,
  since the real DLL's frame length has never been queried on real hardware.
- DeepFilterNet is wired into `noise-engine` and verified against the real
  `tract`-based API by reading its source, but has never been constructed
  or run (no model `.tar.gz` file available in this sandbox) and is not
  reachable from the GUI (RNNoise is the only noise engine the app currently
  exposes).
- The packaged MSVC `.exe` + DLL folder itself does not exist yet — see
  `docs/PACKAGING.md` for the recipe to produce and test it on real Windows.

## Repository layout

- `crates/dsp-core` — portable core: bypass toggles, ring buffers, latency
  metrics/status classification.
- `crates/studio-dsp` — EQ/compressor/de-esser/limiter, matched to the real
  reference's shipped preset values.
- `crates/noise-engine` — RNNoise (`nnnoiseless`) / DeepFilterNet (`deep_filter`)
  registry.
- `crates/bvc-hush` — real `weya_nc.dll` FFI wrapper, loaded at runtime.
- `crates/aec` — NLMS adaptive-filter echo canceller (webrtc-audio-processing
  bundled build confirmed infeasible in this environment).
- `crates/audio-io` — WASAPI capture/render/loopback, VB-Cable endpoint
  discovery.
- `app` — the single `clearnairt.exe`: `iced`-based GUI + engine, one process.
- `docs/VIRTUAL_DEVICES.md`, `docs/PACKAGING.md`, `PHASE0.md` — decisions and
  setup steps.

## What I'd recommend the user (or a Windows-side session) do next

1. Run the MSVC build (`docs/PACKAGING.md` step 1) and report whether it
   succeeds — this is the single biggest open unknown.
2. If it builds, run `clearnairt.exe`, install one VB-Cable pair, assign it
   to one role, and confirm the app doesn't crash and the toggles visibly
   respond.
3. Get the real `weya_nc.dll` + model bundle next to the exe and confirm BVC
   goes from greyed-out to available.
4. Only after 2-3 work, attempt the two-cable setup and the real
   mic/speaker/loopback/AEC verifications listed above.
