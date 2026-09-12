# Phase 7 — Packaging (to be run ON WINDOWS, not here)

This repo was developed and cross-checked on Linux (no MSVC, no Windows
hardware). The actual shippable build must be produced with the real MSVC
toolchain on a Windows machine, per the project spec. This document is the
recipe for that step; it has not been run.

## 1. Build

```powershell
rustup target add x86_64-pc-windows-msvc   # if not already present
cargo build --release --target x86_64-pc-windows-msvc -p clearnai-app
```

This produces `target\x86_64-pc-windows-msvc\release\clearnairt.exe`.

Note: everything in this repo was cross-checked (and, for the GNU target
only, actually built and linked) against `x86_64-pc-windows-gnu` from Linux,
since no MSVC linker exists here. A clean GNU-target build/link is strong
evidence the code is portable and free of GNU-specific accidents, but it is
**not** proof the MSVC build will succeed — different linker, different
import libraries, different `cc`-crate behavior for `nnnoiseless` (has no C
build) and `tract-linalg` (does hand-write x86_64 assembly via `cc::Build`
for its SIMD kernels — confirmed to need a working C toolchain; MSVC's `cl.exe`
is a different C compiler than GCC/mingw and has not been exercised here).
**Run the MSVC build yourself and report back if it fails** — do not assume
success carries over from the GNU result.

## 2. Gather the folder

`clearnairt.exe` is now fully self-contained for BVC purposes: `weya_nc.dll`
and the ONNX model bundle (`advanced_dfnet16k_model_best_onnx.tar.gz`) are
embedded directly into the binary at build time via `include_bytes!` (see
`app/assets/` and `app/src/setup.rs::ensure_bvc_assets_extracted`) and are
self-extracted into `%LOCALAPPDATA%\ClearNAI` the first time the app runs
(and re-extracted automatically if either ever goes missing or comes back a
different size — e.g. after an app update that bundles a newer version).
There is nothing left to copy alongside the exe for BVC to work — no more
manually placing a `.dll` or a model archive next to it:

```powershell
$dist = "dist\ClearNAI"
New-Item -ItemType Directory -Force -Path $dist
Copy-Item target\x86_64-pc-windows-msvc\release\clearnairt.exe $dist\

# That's it for BVC — weya_nc.dll and the model bundle are embedded inside
# clearnairt.exe itself and self-extract into %LOCALAPPDATA%\ClearNAI on
# first run. If extraction ever fails (disk full, unwritable
# %LOCALAPPDATA%, etc.), the app still starts and shows BVC as
# unavailable with a clear error on the setup screen, exactly like the old
# "DLL not found" case.

# rustc/cargo will have already statically linked everything else this
# project depends on (nnnoiseless, deep_filter/tract, wasapi, iced/wgpu) —
# confirm by running `dumpbin /dependents clearnairt.exe` (or `ldd`-equivalent,
# e.g. "Dependencies" GUI tool) on the real Windows build and copying any
# non-system DLL it reports that isn't already in $dist. This has not been
# done here since no MSVC-built exe exists yet to inspect.
```

## 3. Driver / virtual-device install step (separate from the app folder)

This does **not** ship inside the app folder and is **not** a one-click
step — it is a privileged, one-time setup the end user (or an installer
script you write separately) must do before ClearNAI's virtual devices are
selectable in other apps:

1. Install **two independent VB-Audio Virtual Cable instances** (see
   `docs/VIRTUAL_DEVICES.md` for why one isn't enough — e.g. VB-Audio's
   "VB-CABLE A+B" package, or two separate cable-family installs).
2. Each VB-Cable install runs its own Windows driver installer (from
   vb-audio.com) requiring admin rights and, on some Windows versions, a
   reboot or a driver-signature prompt. This is entirely outside this
   project's code — we only consume the resulting WASAPI endpoints once
   installed.
3. Launch `clearnairt.exe`, and in its GUI use the "Virtual mic target" /
   "Virtual speaker source" device pickers to assign each cable's correct
   endpoint (see `docs/VIRTUAL_DEVICES.md`).

## 4. What's NOT yet verified about this packaged build

Per this project's working principles, stated plainly rather than implied:
- The MSVC build itself: not run.
- The packaged folder launching on a clean machine without a Rust
  toolchain: not run.
- Every claim in the main status report about virtual mic/speaker/loopback/
  AEC/GUI-toggle behavior applies here unchanged — none of it has been
  observed on real hardware.
