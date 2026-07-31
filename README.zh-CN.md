# taskmgr-rs

[English](README.md) | 简体中文

用 Rust 和原生 Win32 API 写的 Windows 任务管理器。界面沿用经典任务管理器的布局，系统采样和后台刷新则按现在的 Windows 接口重新实现。

它不是现代任务管理器的复刻，也不靠 CPU 或 GPU 型号表猜数据。系统没有返回某项信息时，页面会直接显示不可用；刷新失败时，上一份有效结果仍会留在界面上。

## 目前有这些页面

- **应用程序**：查看顶层窗口，切换、最小化、最大化或结束任务。
- **进程**：虚拟列表、列排序与拖动、用户名和会话信息，以及结束进程、结束进程树、优先级和亲和性等操作。涉及进程的操作会同时校验 PID 和创建时间。
- **性能**：总 CPU、每逻辑处理器和内存历史图。每核标签可以显示 processor group、核心效率等级和 SMT 线程。
- **CPU**：处理器型号、有效频率、拓扑、缓存、虚拟化、ISA、系统速率计数和 60 秒总利用率历史。
- **GPU**：多适配器、多引擎、专用/共享显存、温度、驱动和 DirectX 信息。
- **网络与用户**：网卡吞吐历史、会话状态和常用会话操作。

## 下载

最新版本在 [Releases](https://github.com/JamesLinYJ/taskmgr-rs/releases/latest)：

- [Windows x86_64](https://github.com/JamesLinYJ/taskmgr-rs/releases/latest/download/taskmgr-windows-x86_64.exe)，适合常见的 Intel 和 AMD Windows 电脑。
- [Windows ARM64](https://github.com/JamesLinYJ/taskmgr-rs/releases/latest/download/taskmgr-windows-arm64.exe)，用于 Windows on Arm 设备。

两个版本都是单文件 EXE，不需要安装。程序会申请管理员权限，因为部分进程和会话操作需要它。

目前发布文件没有代码签名，Windows 可能会显示 SmartScreen 提示。每个版本的发布说明都列出了 SHA-256，可以下载后自行核对。

## 运行环境

项目主要在 Windows 11 x86_64 上开发和实测。[兼容性矩阵](docs/compatibility-matrix.md)
分别记录 Windows/架构与 GPU 的可复核证据，不会把交叉编译或未留下环境记录的开发机当成
硬件实测。

GPU 页面依赖系统和显卡驱动公开的 `GPU Engine` 与 `GPU Adapter Memory` 性能计数器。计数器不存在或驱动没有返回数据时，页面会说明具体状态，不会把查询失败画成 0%。

ARM64 版本已经通过交叉编译、全目标检查和 PE 架构检查，但目前没有在 ARM64 实机上跑过。

## 从源码编译

需要稳定版 Rust、MSVC C++ Build Tools 和 Windows SDK。仓库的默认目标是 `x86_64-pc-windows-msvc`。

```powershell
cargo build --release
```

编译 32 位 x86 可执行文件：

```powershell
rustup target add i686-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc
# 也可以运行：.\scripts\release-clean.ps1 -Target i686-pc-windows-msvc
```

x86 构建仍会同时查询进程机器类型和系统原生机器类型：在 64 位 Windows 的 WOW64 下
继续标注“(32位)”，只有在原生 32 位 Windows 上才省略该后缀。

需要路径重映射的发布构建时运行：

```powershell
.\scripts\release-clean.ps1
```

编译 ARM64 前，还要安装 Visual Studio 的 C++ ARM64 Build Tools：

```powershell
rustup target add aarch64-pc-windows-msvc
.\scripts\release-clean.ps1 -Target aarch64-pc-windows-msvc
```

生成的文件位于 `target/<target>/release/taskmgr.exe`。

## 诊断日志

程序默认把经过脱敏的基础诊断日志写入
`%LOCALAPPDATA%\taskmgr-rs\logs\<session-id>`。通过“帮助 > 诊断日志...”可以查看当前会话、
为本次运行开启详细日志、带完整启动日志重启、打开日志目录，或保存 ZIP 诊断包。“包含敏感
信息”和崩溃 minidump 都是默认关闭的单次会话选项；minidump 可能包含进程内存。诊断包不会
自动上传，也不包含截图、注册表配置或完整进程清单。

对应的命令行参数如下：

```text
--diagnostic
--diagnostic=debug
--diagnostic=trace
--diagnostic-sensitive
--diagnostic-minidump
--diagnostic-dir=<绝对路径>
--diagnostic-capabilities=<新的 JSON 文件绝对路径>
```

日志采用带版本的 JSON Lines 格式，单文件达到 10 MiB 后轮转，最多保留 10 个会话和
200 MiB。常规目录不可用时会回退到 `%TEMP%`；两个目录都失败时仍保留原生调试器输出。

能力导出命令会查询 processor group、CPU Set、DXGI/驱动元数据以及程序实际使用的 GPU
PDH 计数器路径，写出可直接附在 Issue 中的 JSON，随后退出而不打开主界面。不支持的能力和
原生错误码都会明确保留。目标必须是尚不存在的绝对路径；用法、隐私说明和完整验证步骤见
[兼容性矩阵与检查清单](docs/compatibility-matrix.md)。

在 Linux 上用 MinGW GNU 工具链生成优化后的 EXE、分离 DWARF 符号和匹配的 SHA-256：

```bash
./scripts/build-diagnostics.sh
```

Windows 上运行 `.\scripts\build-diagnostics.ps1` 会生成优化后的 MSVC EXE、单独的 PDB 和
SHA-256。`.debug` 与 PDB 只保留在开发端；诊断包通过可执行文件哈希匹配符号。

开发者验证错误链路时，可在启动前设置
`TASKMGR_RS_DIAGNOSTIC_INJECT_SAMPLING_ERROR=ntstatus-once` 并同时传入 `--diagnostic=trace`。
随后第一次手动“查看 > 立即刷新”会注入一次受控的 `STATUS_UNSUCCESSFUL`；后续采样自动恢复。
该开关不写入配置，在普通或非详细启动下不会生效。

验证崩溃符号链时，可设置 `TASKMGR_RS_DIAGNOSTIC_TEST_CRASH=access-violation`，并同时传入
`--diagnostic=trace --diagnostic-minidump`。这会在崩溃记录器初始化后故意终止进程，仅应在
隔离的测试运行中使用；普通、非详细或未启用 minidump 的启动会忽略该开关。

## 数据从哪里来

CPU 拓扑来自 `GetLogicalProcessorInformationEx`，动态频率和系统速率使用 PDH，固件信息来自 WMI。GPU 适配器由 DXGI 枚举，引擎和显存使用 PDH，驱动属性来自 SetupAPI；温度等适配器数据只读取受支持的 `D3DKMTQueryAdapterInfo` 类型。

项目不使用厂商 SDK、CPU 型号表或 `D3DKMTQueryStatistics`。不同来源的数据会分开提交，一个来源出错不会把整页已有内容清空。

## 目录结构

项目仍是一个 Rust crate。目录按资源所有权和线程边界组织，不会为了拆文件而给每个小类型单独建模块：

- `src/app` 管理主窗口、页面注册表、公共对话框宿主和应用级控制器。
- `src/infrastructure` 放有界 single-flight worker，以及各功能都会用到的小型 Win32 RAII 封装。
- `src/system` 负责系统采样、处理器拓扑和稳定进程身份。
- `src/pages` 保存各页面状态和页面专属数据源。CPU、GPU 的原生查询、性能计数器和元数据有不同生命周期与错误状态，因此分别成模块；网络和用户页职责还不复杂，继续保留单文件。
- `src/ui` 放图表、对话框、菜单、绘制、资源和本地化等通用 UI 代码。
- `src/config` 负责带版本的注册表配置格式。

后台 worker 每次只提交一个来源的完整快照。UI 会拒绝过期 generation；刷新失败时保留上一份有效结果。Win32、PDH、DXGI、WMI、WTS 和图标采集等可能阻塞的工作不会放到 UI 线程。

## 图像资源

仓库中的图片源文件只使用 PNG 或 BMP，并统一放在 `assets` 下。构建脚本会在 Cargo 的 `OUT_DIR` 中临时生成 ICO，再由 `winresource` 嵌入 EXE；仓库和发布目录都不会保留单独的 `.ico` 文件。

修改图标时必须一次交齐清单中的所有尺寸。构建过程不会缩放图片，也不会拿相近文件顶替：

- `application-{16,20,24,32,40,48,64,256}.png`
- `default-process-{16,20,24,32,40,48,64}.png`
- `cpu-usage-level-{00..11}-{16,20,24,32}.png`

每张 PNG 都必须是正方形 RGBA 图片，实际边长要和文件名一致。仪表位图位于 `assets/bitmaps`，应用 manifest 位于 `assets/windows/taskmgr.manifest`。数值资源 ID 和预期文件清单以 `build_support/resources.rs` 为准。

图片修改后先运行 `cargo test --test resource_assets`。release 构建还会再次校验完整资源集，然后才生成临时 ICO 并编译最终的单文件 EXE。

## 提交前检查

```powershell
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
git diff --check
```

## 反馈问题

可以直接开 [Issue](https://github.com/JamesLinYJ/taskmgr-rs/issues)。如果是采样或布局问题，请附上 Windows 版本、CPU/GPU 型号、出问题的页面、复现步骤和截图；兼容性问题可附上能力 JSON。需要精确追踪 API 与源码行时，可以先查看隐私提示，再附上诊断包。这几项通常比一句“显示不对”更容易把问题查清楚。

## 许可证

[MIT](LICENSE)，版权归 taskmgr-rs contributors。
