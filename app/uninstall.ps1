# Uninstalls ClearNAI for Windows.
#
# ClearNAI is a portable, single-exe app: it has no installer, writes
# nothing to the Windows Registry, and never touches Program Files. The
# only things it ever writes to disk are:
#   - %LOCALAPPDATA%\ClearNAI\weya_nc.dll               (self-extracted BVC DLL)
#   - %LOCALAPPDATA%\ClearNAI\advanced_dfnet16k_model_best_onnx.tar.gz  (self-extracted BVC model)
#   - %LOCALAPPDATA%\ClearNAI\settings.json              (your saved preferences)
#   - %LOCALAPPDATA%\ClearNAI\clearnai.log               (diagnostic log)
# This script removes exactly that folder, then (optionally) deletes
# clearnairt.exe itself if it's sitting next to this script.
#
# What this does NOT touch, on purpose:
#   - Any VB-Audio Virtual Cable / VoiceMeeter driver install. Those are
#     separate third-party products you installed yourself (see
#     docs/VIRTUAL_DEVICES.md) and may be in use by other apps - uninstall
#     them yourself via Windows Settings > Apps, only if you're sure you
#     don't need them elsewhere.
#
# Usage: right-click this file -> "Run with PowerShell", or from a
# PowerShell prompt: powershell -ExecutionPolicy Bypass -File uninstall.ps1

$ErrorActionPreference = "Stop"

Write-Host "ClearNAI uninstaller"
Write-Host "===================="

if (-not $env:LOCALAPPDATA) {
    Write-Host "LOCALAPPDATA is not set on this system; nothing to remove there."
} else {
    $dataDir = Join-Path $env:LOCALAPPDATA "ClearNAI"
    if (Test-Path $dataDir) {
        Write-Host "Removing $dataDir (settings, log, self-extracted BVC assets)..."
        Remove-Item -Recurse -Force $dataDir
        Write-Host "Done."
    } else {
        Write-Host "$dataDir does not exist; nothing to remove there."
    }
}

$exePath = Join-Path $PSScriptRoot "clearnairt.exe"
if (Test-Path $exePath) {
    $confirm = Read-Host "Also delete $exePath ? [y/N]"
    if ($confirm -eq "y" -or $confirm -eq "Y") {
        Remove-Item -Force $exePath
        Write-Host "Deleted $exePath."
    } else {
        Write-Host "Left $exePath in place."
    }
} else {
    Write-Host "clearnairt.exe not found next to this script; skipping."
}

Write-Host ""
Write-Host "Note: if you installed VB-Audio Virtual Cable, VoiceMeeter, or any other"
Write-Host "virtual audio driver for use with ClearNAI, this script does NOT remove"
Write-Host "them - uninstall those separately via Windows Settings > Apps if you no"
Write-Host "longer need them."
Write-Host ""
Write-Host "Uninstall complete."
