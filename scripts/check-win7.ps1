param(
    [switch]$SkipRelease
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$Toolchain = "1.77.2"
$Target = "x86_64-pc-windows-msvc"

Write-Host "Checking Windows 7 SP1 compatibility build with Rust $Toolchain ($Target)"

& cargo "+$Toolchain" fmt -- --check
& cargo "+$Toolchain" check --target $Target
& cargo "+$Toolchain" test --offline --target $Target

if (-not $SkipRelease) {
    & cargo "+$Toolchain" build --release --target $Target

    $Exe = Join-Path $Root "target\$Target\release\taskmgr.exe"
    if (-not (Test-Path -LiteralPath $Exe)) {
        throw "Release executable not found: $Exe"
    }

    Write-Host ""
    Write-Host "Imported DLLs for manual Win7 review:"
    $Dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($Dumpbin) {
        & $Dumpbin.Source /DEPENDENTS $Exe
    } else {
        Write-Host "dumpbin.exe was not found on PATH. Run this from a Visual Studio Developer PowerShell to list imports."
        Write-Host "Executable: $Exe"
    }
}
