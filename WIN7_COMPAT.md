# Windows 7 SP1 Compatibility

This project targets Windows 7 SP1 x64 release compatibility.

## Build Baseline

- Rust toolchain: `1.77.2`
- Target: `x86_64-pc-windows-msvc`
- Release command: `cargo +1.77.2 build --release --target x86_64-pc-windows-msvc`

Rust 1.78 and newer raised the normal `*-pc-windows-*` runtime baseline to Windows 10. The project pins Rust 1.77.2 in `rust-toolchain.toml` so release binaries keep the older Windows 7-compatible baseline.

## Compatibility Audit Notes

- `sysmon.manifest` already declares the Windows 7 supportedOS GUID.
- Direct2D is available on Windows 7; the chart renderer falls back to GDI when Direct2D initialization or frame binding fails.
- `GetIfTable2` / `MIB_IF_ROW2` are Vista-era APIs and are acceptable for Windows 7 SP1.
- `QueryFullProcessImageNameW`, `K32*` process APIs, WTS APIs, `GetSystemTimes`, and `GlobalMemoryStatusEx` are available on Windows 7.
- The app is scoped to Windows 7 SP1 x64. Windows 7 RTM, Vista, XP, and 32-bit builds are not covered by this compatibility baseline.

## Verification

Run:

```powershell
.\scripts\check-win7.ps1
```

The script runs:

- `cargo +1.77.2 fmt -- --check`
- `cargo +1.77.2 check --target x86_64-pc-windows-msvc`
- `cargo +1.77.2 test --offline --target x86_64-pc-windows-msvc`
- `cargo +1.77.2 build --release --target x86_64-pc-windows-msvc`

When run from a Visual Studio Developer PowerShell, it also prints imported DLLs with `dumpbin /DEPENDENTS` for manual review.

## Manual Smoke Test

Use a Windows 7 SP1 x64 VM and test:

- Launch the release executable.
- Switch through all five pages.
- Confirm performance and networking graphs draw.
- Open tray menu and restore/minimize behavior.
- Refresh, then exit and relaunch to verify options save/load.
