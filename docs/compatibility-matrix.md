# Windows and hardware compatibility matrix

This matrix records evidence, not assumed support. A row is **verified** only after the checklist
below has been run on that exact Windows build, architecture, and hardware. Compilation, a
capability query, and hands-on UI testing are recorded separately.

## Status

- **Verified**: the complete checklist passed on the stated machine.
- **Capability verified**: the native capability report succeeded, but the UI checklist was not
  completed.
- **Build only**: compilation and automated checks passed; no hardware run is claimed.
- **Pending**: no reproducible evidence has been recorded.

## Windows and architecture coverage

| Windows release | Architecture | Build/check evidence | Capability report | Hands-on UI | Status |
| --- | --- | --- | --- | --- | --- |
| Windows 10 | x86_64 | MSVC release target built on 2026-07-31; no recorded stable Windows 10 run | Pending | Pending | Build only |
| Windows 10 | ARM64 | MSVC ARM64 all-target check and release build passed on 2026-07-31 | Pending | Pending | Build only |
| Windows 11 | x86_64 | MSVC release target built on 2026-07-31; historical development use has no exact stable-build checklist record | Pending on a recorded stable build | Pending on a recorded stable build | Build only |
| Windows 11 | ARM64 | MSVC ARM64 all-target check and release build passed on 2026-07-31 | Pending | Pending | Build only |
| Windows Dev channel, `RtlGetVersion` 10.0.29634 | x86_64 | Full repository quality gates and MSVC release build passed, 2026-07-31 | CPU Sets, DXGI, SetupAPI, D3DKMT, and all three GPU PDH paths returned supported | Live GPU source integration passed; full UI checklist pending | Capability verified |
| Windows Dev channel, `RtlGetVersion` 10.0.29634 | x86 under WOW64 | i686 all-target check, Clippy, full tests, release build, and PE `I386` verification passed, 2026-07-31 | Capability command exited 0; one processor group and two DXGI adapters were enumerated | Full UI checklist pending; no native 32-bit Windows run | Capability verified |

The Dev-channel row is not evidence for a stable Windows 10 or Windows 11 release. Windows reports
the NT version as 10.0 for both product families, so a build number must always accompany the
marketing release name.

## GPU and driver coverage

| Date | Windows build / arch | Vendor and adapter | Driver | GPU Engine | Dedicated usage | Shared usage | Metadata / temperature | UI | Status |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 2026-07-31 | Dev 29634 / x86_64 | NVIDIA GeForce RTX 5060 Laptop GPU | 32.0.16.1062 | Supported | Supported | Supported | Supported / supported | Live source integration passed; manual UI pending | Capability verified |
| 2026-07-31 | Dev 29634 / x86_64 | AMD Radeon 610M | 32.0.21045.1000 | Supported | Supported | Supported | Supported / supported | Live source integration passed; manual UI pending | Capability verified |
| — | — | Intel | — | Pending | Pending | Pending | Pending | Pending | Pending |

The counter columns mean that `PdhAddEnglishCounterW` accepted the exact paths used by the GPU
page. They do not claim that every adapter instance produced a valid sample; the UI checklist must
also confirm per-adapter updates and partial-failure behavior.

## Generate an attachable capability report

Use a new absolute destination filename. The command runs the real Windows queries, writes JSON,
and exits without opening the main window:

```powershell
& .\taskmgr.exe "--diagnostic-capabilities=$env:TEMP\taskmgr-rs-capabilities.json"
```

The command reports:

- application version, target architecture/ABI, Windows build, WOW64 state, and elevation state;
- active processor groups, group-relative logical processors, CPU Set IDs, assignability, and the
  current process defaults;
- DXGI adapter identity, vendor/device IDs, memory limits, display-driver version/date, DirectX
  feature level, and D3DKMT temperature-query availability;
- availability of the exact `GPU Engine`, dedicated-memory, and shared-memory PDH paths;
- `supported`, `unsupported`, or `error` for each query, preserving its Win32, HRESULT, NTSTATUS, or
  PDH error code and operation context.

The destination must not already exist. Its parent path is opened and validated by handle, reparse
points are rejected, a cryptographically random temporary file is created exclusively with a
restrictive ACL, and the completed report is renamed atomically without overwriting the
destination. No report is uploaded automatically.

Review the report before attaching it: it contains CPU topology, GPU model identifiers, and driver
versions, but no process list.

## Compatibility checklist

Record the commit, exact Windows build, architecture, CPU model/logical-processor count, GPU models,
and driver versions before starting.

1. Run the repository quality gates and record whether the build was native or cross-compiled:
   `cargo fmt --all -- --check`, `cargo check --all-targets`,
   `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, and
   `cargo build --release`.
2. Generate a capability JSON with a fresh filename. Confirm every unsupported/error entry remains
   explicit and includes an error domain, code, and context.
3. Start the release executable. Exercise every page, manual refresh, minimize/restore, tray
   restore, and clean exit.
4. On the GPU page, select every adapter. Confirm healthy adapter histories keep advancing and that
   unavailable engine/dedicated/shared/temperature sources display `N/A` or partial/stale status
   rather than a fabricated zero.
5. On a machine with more than one processor group, open process affinity, select CPUs from each
   group, apply, reopen the dialog, and independently verify the target process/thread group
   assignments.
6. Exercise diagnostic log creation, retention, and ZIP export in a user-writable directory.
   Confirm an existing output still requires the save-dialog overwrite confirmation.
7. Attach the capability JSON and note any UI-only failure. Do not mark a row **Verified** until all
   applicable steps pass.

When a capability is genuinely absent, record **Unsupported** rather than failing the whole machine
row. When a query unexpectedly fails, record **Error** with its native code and keep the row open
until the cause is understood.
