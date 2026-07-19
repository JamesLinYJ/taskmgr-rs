param(
    [switch]$UseNightlyBuildStd,
    [ValidateSet("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")]
    [string]$Target = "x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"

function Add-RemapPrefix {
    param(
        [System.Collections.Generic.List[string]]$Flags,
        [string]$From,
        [string]$To
    )

    if ([string]::IsNullOrWhiteSpace($From)) {
        return
    }

    $normalized = $From.TrimEnd('\', '/')
    if ([string]::IsNullOrWhiteSpace($normalized)) {
        return
    }

    $Flags.Add("--remap-path-prefix=$normalized=$To")
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$toolchain = if ($UseNightlyBuildStd) { "nightly" } else { "" }
$rustflags = New-Object 'System.Collections.Generic.List[string]'

Add-RemapPrefix $rustflags $repoRoot "."
Add-RemapPrefix $rustflags $env:USERPROFILE "C:\\Users\\builder"
Add-RemapPrefix $rustflags $env:CARGO_HOME "C:\\.cargo"
Add-RemapPrefix $rustflags $env:RUSTUP_HOME "C:\\.rustup"

$sysroot = if ($UseNightlyBuildStd) {
    (rustup run nightly rustc --print sysroot).Trim()
} else {
    (rustc --print sysroot).Trim()
}
$sysrootAlias = if ($UseNightlyBuildStd) {
    "C:\\.rustup\\toolchains\\nightly"
} else {
    "C:\\.rustup\\toolchains\\stable"
}
Add-RemapPrefix $rustflags $sysroot $sysrootAlias

$rustSrc = Join-Path $sysroot "lib\\rustlib\\src\\rust"
if (Test-Path $rustSrc) {
    Add-RemapPrefix $rustflags $rustSrc "/rustc/src"
}

$existing = $env:RUSTFLAGS
if ($existing) {
    $rustflags.Insert(0, $existing)
}
if ($UseNightlyBuildStd) {
    $rustflags.Add("-Zunstable-options")
    $rustflags.Add("-Cpanic=immediate-abort")
}
$env:RUSTFLAGS = ($rustflags -join " ").Trim()

Push-Location $repoRoot
try {
    if ($UseNightlyBuildStd) {
        $hasNightly = ((rustup toolchain list) | Select-String '^nightly').Length -gt 0
        if (-not $hasNightly) {
            throw "nightly toolchain is not installed"
        }

        cargo +nightly build `
            -Z build-std=std,panic_abort `
            --target $Target `
            --release
    } else {
        cargo build --target $Target --release
    }
} finally {
    Pop-Location
}
