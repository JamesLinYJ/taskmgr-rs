# +-------------------------------------------------------------------------
#
#   taskmgr-rs - MSVC 诊断符号构建
#
#   文件:       scripts/build-diagnostics.ps1
#
#   日期:       2026年07月27日
#   环境:       Fedora Linux 45 x86_64；Linux 内核 7.2.0-0.rc4.260725g0ce37745d4bf.39.fc45.x86_64；Rust 1.97.1；MinGW GCC 16.1.1；Wine 11.14 (Staging)
#   作者:       OpenAI Codex
# --------------------------------------------------------------------------

param(
    [ValidateSet(
        "x86_64-pc-windows-msvc",
        "i686-pc-windows-msvc",
        "aarch64-pc-windows-msvc"
    )]
    [string]$Target = "x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$profileDir = Join-Path $repoRoot "target\$Target\diagnostics"
$executable = Join-Path $profileDir "taskmgr.exe"
$pdb = Join-Path $profileDir "taskmgr.pdb"
$hashFile = Join-Path $profileDir "taskmgr.exe.sha256"
$separator = [char]0x1f
$previousEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS
$previousRustFlags = $env:RUSTFLAGS
$cargoCommand = if ($env:CARGO) { $env:CARGO } else { "cargo" }

$flags = @(
    "-C",
    "debuginfo=2",
    "-C",
    "link-arg=/DEBUG:FULL",
    "-C",
    "link-arg=/PDB:$pdb",
    "-C",
    "link-arg=/PDBALTPATH:%_PDB%"
)
$env:CARGO_ENCODED_RUSTFLAGS = $flags -join $separator
Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue

Push-Location $repoRoot
try {
    & $cargoCommand build --profile diagnostics --target $Target
    if (-not (Test-Path $pdb)) {
        throw "The linker did not produce the expected PDB: $pdb"
    }
    $hash = (Get-FileHash -Algorithm SHA256 $executable).Hash.ToLowerInvariant()
    "$hash  taskmgr.exe" | Set-Content -Encoding ascii $hashFile
} finally {
    Pop-Location
    if ($null -eq $previousEncodedFlags) {
        Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    } else {
        $env:CARGO_ENCODED_RUSTFLAGS = $previousEncodedFlags
    }
    if ($null -eq $previousRustFlags) {
        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    } else {
        $env:RUSTFLAGS = $previousRustFlags
    }
}

Write-Host "Executable: $executable"
Write-Host "Symbols:    $pdb"
Write-Host "SHA-256:    $hashFile"
