# taskmgr-rs

English | [简体中文](README.zh-CN.md)

A Windows task manager written in Rust with native Win32 APIs. The UI follows the older Task Manager layout, while sampling and background refresh use current Windows interfaces.

This is not a copy of the modern Task Manager. CPU and GPU details come from system APIs rather than model lookup tables. If Windows does not return a value, the UI says it is unavailable. A failed refresh also leaves the last valid result on screen instead of replacing it with zeros.

## Pages

- **Applications** lists top-level windows and can switch to, minimize, maximize, or end a task.
- **Processes** uses a virtual list with sortable and movable columns. Process actions verify both the PID and creation time before they run.
- **Performance** shows CPU, per-logical-processor, and memory history. CPU labels can include processor groups, efficiency classes, and SMT thread numbers.
- **CPU** shows the processor name, effective frequency, topology, caches, virtualization, ISA features, system rates, and a 60-second utilization graph.
- **GPU** handles multiple adapters and engines, dedicated and shared memory, temperature, driver details, and DirectX information.
- **Networking and Users** show adapter throughput history and Windows session state.

## Download

The current build is on the [Releases page](https://github.com/JamesLinYJ/taskmgr-rs/releases/latest):

- [Windows x86_64](https://github.com/JamesLinYJ/taskmgr-rs/releases/latest/download/taskmgr-windows-x86_64.exe) for most Intel and AMD Windows PCs.
- [Windows ARM64](https://github.com/JamesLinYJ/taskmgr-rs/releases/latest/download/taskmgr-windows-arm64.exe) for Windows on Arm devices.

Both downloads are single EXE files. The program asks for administrator access because some process and session actions require it.

The release files are not code-signed at the moment, so Windows may show a SmartScreen warning. Each release includes SHA-256 hashes for manual verification.

## Windows support

Most development and hands-on testing currently happens on Windows 11 x86_64. The code uses Win32, PDH, DXGI, WMI, and WDDM interfaces available on Windows 10 and 11, but the project does not yet have a complete OS and hardware test matrix.

The GPU page needs the `GPU Engine` and `GPU Adapter Memory` performance counters exposed by Windows and the display driver. When those counters are missing, the page reports that state instead of drawing a flat 0% graph.

The ARM64 release passes cross-compilation, all-target checks, and PE architecture checks. It has not yet been run on ARM64 hardware.

## Building

Install stable Rust, the MSVC C++ Build Tools, and a Windows SDK. The repository defaults to `x86_64-pc-windows-msvc`.

```powershell
cargo build --release
```

For a release build with local paths remapped:

```powershell
.\scripts\release-clean.ps1
```

ARM64 builds also need the Visual Studio C++ ARM64 Build Tools:

```powershell
rustup target add aarch64-pc-windows-msvc
.\scripts\release-clean.ps1 -Target aarch64-pc-windows-msvc
```

The executable is written to `target/<target>/release/taskmgr.exe`.

## Data sources

CPU topology comes from `GetLogicalProcessorInformationEx`. PDH supplies dynamic frequency and system rates, and WMI supplies firmware data. GPU adapters are enumerated through DXGI; engine and memory counters come from PDH; driver properties come from SetupAPI. Temperature and similar adapter data are read only through supported `D3DKMTQueryAdapterInfo` query types.

The project does not use vendor SDKs, CPU model tables, or `D3DKMTQueryStatistics`. Data sources are committed separately, so one failed query does not clear an otherwise usable page.

## Repository layout

The project remains one Rust crate. Directories follow ownership boundaries rather than putting every type in its own file:

- `src/app` owns the main window, page registry, shared dialog host, and application-wide controllers.
- `src/infrastructure` contains the bounded single-flight worker and small Win32 RAII wrappers shared across features.
- `src/system` contains system sampling, processor topology, and stable process identities.
- `src/pages` owns page state and feature-specific data sources. CPU and GPU have separate native, counter, and metadata modules because those sources have different lifetimes and failure states. Networking and Users stay as single modules while their responsibilities remain compact.
- `src/ui` contains reusable chart, dialog, menu, drawing, resource, and localization code.
- `src/config` owns the versioned registry options format.

Workers publish complete snapshots from one source at a time. The UI rejects stale generations and keeps the last valid snapshot when a refresh fails. Expensive Win32, PDH, DXGI, WMI, WTS, and icon work stays off the UI thread.

## Image resources

The repository stores image sources as PNG or BMP files under `assets`. Windows ICO data is generated in Cargo's `OUT_DIR` during the build and embedded into the EXE by `winresource`; no `.ico` source file is kept or shipped beside the program.

Icon contributions must provide every declared size. The build does not resize images or substitute a nearby file:

- `application-{16,20,24,32,40,48,64,256}.png`
- `default-process-{16,20,24,32,40,48,64}.png`
- `cpu-usage-level-{00..11}-{16,20,24,32}.png`

Each PNG must be square RGBA at the size in its filename. Meter bitmaps live in `assets/bitmaps`, and the application manifest lives at `assets/windows/taskmgr.manifest`. The resource manifest in `build_support/resources.rs` is the source of truth for numeric IDs and expected files.

Run `cargo test --test resource_assets` after changing an image. A release build then checks the same source set before writing the temporary ICO files and compiling the final single-file executable.

## Checks

```powershell
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
git diff --check
```

## Reporting a problem

Open an [Issue](https://github.com/JamesLinYJ/taskmgr-rs/issues) and include the Windows version, CPU and GPU models, the affected page, steps to reproduce, and a screenshot. Those details usually make sampling and layout bugs much easier to pin down.

## License

[MIT](LICENSE). Copyright belongs to the taskmgr-rs contributors.
