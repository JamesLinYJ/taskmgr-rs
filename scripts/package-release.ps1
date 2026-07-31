# +-------------------------------------------------------------------------
#
#   taskmgr-rs - 发布资产架构与版本校验
#
#   文件:       scripts/package-release.ps1
#
#   日期:       2026年07月31日
#   环境:       Windows 10 Pro Dev（Build 29634.1000）x86_64；Rust 1.97.0；MSVC 14.50.35729.0
#   作者:       OpenAI Codex
# --------------------------------------------------------------------------

param(
    [Parameter(Mandatory)]
    [ValidateSet(
        "x86_64-pc-windows-msvc",
        "i686-pc-windows-msvc",
        "aarch64-pc-windows-msvc"
    )]
    [string]$Target,

    [Parameter(Mandatory)]
    [string]$Executable,

    [Parameter(Mandatory)]
    [string]$OutputDirectory,

    [string]$ExpectedVersion
)

$ErrorActionPreference = "Stop"

$targetDetails = @{
    "x86_64-pc-windows-msvc" = @{
        Machine = [uint16]0x8664
        Asset = "taskmgr-windows-x86_64.exe"
    }
    "i686-pc-windows-msvc" = @{
        Machine = [uint16]0x014c
        Asset = "taskmgr-windows-x86.exe"
    }
    "aarch64-pc-windows-msvc" = @{
        Machine = [uint16]0xaa64
        Asset = "taskmgr-windows-arm64.exe"
    }
}

function Get-PeMachine {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $stream = [IO.File]::Open(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read
    )
    try {
        if ($stream.Length -lt 64) {
            throw "file is too small to contain a PE header: $Path"
        }

        $reader = [IO.BinaryReader]::new($stream, [Text.Encoding]::ASCII, $true)
        try {
            if ($reader.ReadUInt16() -ne 0x5a4d) {
                throw "file does not have an MZ header: $Path"
            }

            $stream.Position = 0x3c
            $peOffset = [uint64]$reader.ReadUInt32()
            if ($peOffset -gt [uint64]($stream.Length - 6)) {
                throw "PE header offset is outside the file: $Path"
            }

            $stream.Position = [int64]$peOffset
            if ($reader.ReadUInt32() -ne 0x00004550) {
                throw "file does not have a PE signature: $Path"
            }
            return $reader.ReadUInt16()
        } finally {
            $reader.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$executablePath = if ([IO.Path]::IsPathRooted($Executable)) {
    $Executable
} else {
    Join-Path $repoRoot $Executable
}
$executableItem = Get-Item -LiteralPath $executablePath
if ($executableItem.PSIsContainer) {
    throw "executable path names a directory: $($executableItem.FullName)"
}

$metadataText = & cargo metadata `
    --manifest-path (Join-Path $repoRoot "Cargo.toml") `
    --locked `
    --no-deps `
    --format-version 1
if ($LASTEXITCODE -ne 0) {
    throw "cargo metadata failed with exit code $LASTEXITCODE"
}
$metadata = $metadataText | ConvertFrom-Json
$package = @($metadata.packages) |
    Where-Object { $_.name -eq "taskmgr-rs" } |
    Select-Object -First 1
if ($null -eq $package) {
    throw "cargo metadata did not contain taskmgr-rs"
}
$version = [string]$package.version
if ($ExpectedVersion -and $version -cne $ExpectedVersion) {
    throw "Cargo package version $version does not match expected version $ExpectedVersion"
}

$targetDetail = $targetDetails[$Target]
$machine = Get-PeMachine -Path $executableItem.FullName
if ($machine -ne $targetDetail.Machine) {
    throw (
        "PE machine mismatch for {0}: expected 0x{1:X4}, got 0x{2:X4}" -f
        $Target,
        $targetDetail.Machine,
        $machine
    )
}

$fileVersion = $executableItem.VersionInfo.FileVersion.Trim()
$productVersion = $executableItem.VersionInfo.ProductVersion.Trim()
if ($fileVersion -cne $version) {
    throw "file version $fileVersion does not match Cargo package version $version"
}
if ($productVersion -cne $version) {
    throw "product version $productVersion does not match Cargo package version $version"
}

$outputPath = if ([IO.Path]::IsPathRooted($OutputDirectory)) {
    $OutputDirectory
} else {
    Join-Path $repoRoot $OutputDirectory
}
$outputItem = New-Item -ItemType Directory -Path $outputPath -Force
$assetPath = Join-Path $outputItem.FullName $targetDetail.Asset
Copy-Item -LiteralPath $executableItem.FullName -Destination $assetPath -Force

$assetItem = Get-Item -LiteralPath $assetPath
$sha256 = (
    Get-FileHash -Algorithm SHA256 -LiteralPath $assetItem.FullName
).Hash.ToLowerInvariant()

if ($env:GITHUB_OUTPUT) {
    @(
        "asset_name=$($assetItem.Name)"
        "asset_path=$($assetItem.FullName)"
        "machine=0x$($machine.ToString('X4'))"
        "sha256=$sha256"
        "size=$($assetItem.Length)"
        "version=$version"
    ) | Out-File -FilePath $env:GITHUB_OUTPUT -Encoding utf8 -Append
}

[pscustomobject]@{
    Target = $Target
    Machine = "0x$($machine.ToString('X4'))"
    Version = $version
    Asset = $assetItem.FullName
    Size = $assetItem.Length
    SHA256 = $sha256
} | Format-List
