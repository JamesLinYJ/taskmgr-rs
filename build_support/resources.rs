// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Windows 资源编译
//
//   文件:       build_support/resources.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 校验源码资产、生成临时图标并编译 Windows PE 资源。
//!
//! 资源数值 ID 保持稳定。候选图标全部生成成功后才交给资源编译器，避免产生只有
//! 部分图标或缺少版本信息的可执行文件。

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::icon_pipeline::{IconSource, IconSpec, generate_icon};
use crate::resource::{
    IDB_METER_LIT_GREEN, IDB_METER_LIT_RED, IDB_METER_UNLIT, IDI_APPLICATION, IDI_DEFAULT_PROCESS,
    TRAY_ICON_IDS,
};

const APPLICATION_ICON_SIZES: &[u32] = &[16, 20, 24, 32, 40, 48, 64, 256];
const DEFAULT_PROCESS_ICON_SIZES: &[u32] = &[16, 20, 24, 32, 40, 48, 64];
const TRAY_ICON_SIZES: &[u32] = &[16, 20, 24, 32];

pub(crate) fn compile() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=assets");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env != "msvc" {
        panic!("taskmgr-rs Windows resources require the MSVC target environment");
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let assets_dir = manifest_dir.join("assets");
    reject_source_ico_files(&assets_dir).unwrap_or_else(|error| panic!("{error}"));

    let generated_dir =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("generated-resources");
    fs::create_dir_all(&generated_dir).unwrap_or_else(|error| {
        panic!(
            "failed to create generated resource directory {}: {error}",
            generated_dir.display()
        )
    });

    let icon_specs = icon_specs(&assets_dir);
    let mut compiled_icons = Vec::with_capacity(icon_specs.len());
    for (resource_id, spec) in &icon_specs {
        for source in &spec.sources {
            println!("cargo:rerun-if-changed={}", source.path.display());
        }
        let generated = generate_icon(&generated_dir, spec)
            .unwrap_or_else(|error| panic!("failed to generate icon resource: {error}"));
        compiled_icons.push((*resource_id, generated));
    }

    let bitmap_resources = [
        (
            IDB_METER_LIT_GREEN,
            assets_dir.join("bitmaps/meter-segment-lit-green.bmp"),
        ),
        (
            IDB_METER_LIT_RED,
            assets_dir.join("bitmaps/meter-segment-lit-red.bmp"),
        ),
        (
            IDB_METER_UNLIT,
            assets_dir.join("bitmaps/meter-segment-unlit.bmp"),
        ),
    ];
    for (_, path) in &bitmap_resources {
        println!("cargo:rerun-if-changed={}", path.display());
        validate_meter_bitmap(path).unwrap_or_else(|error| panic!("{error}"));
    }

    validate_resource_ids(
        compiled_icons.iter().map(|(id, _)| *id),
        bitmap_resources.iter().map(|(id, _)| *id),
    )
    .unwrap_or_else(|error| panic!("{error}"));

    let mut rc_content = String::new();
    for (resource_id, path) in &compiled_icons {
        writeln!(rc_content, "{resource_id} ICON \"{}\"", rc_path(path)).unwrap();
    }
    for (resource_id, path) in &bitmap_resources {
        writeln!(rc_content, "{resource_id} BITMAP \"{}\"", rc_path(path)).unwrap();
    }

    let mut resources = winresource::WindowsResource::new();
    resources.append_rc_content(&rc_content);
    resources
        .compile()
        .unwrap_or_else(|error| panic!("failed to compile Windows resources: {error}"));

    let manifest_path = assets_dir.join("windows/taskmgr.manifest");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg=/MANIFESTINPUT:{}",
        manifest_path.display()
    );
    if env::var("PROFILE").unwrap_or_default() == "release" {
        println!("cargo:rustc-link-arg=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'");
    }
}

fn icon_specs(assets_dir: &Path) -> Vec<(u16, IconSpec)> {
    let mut specs = vec![
        (
            IDI_APPLICATION,
            named_icon_spec(
                assets_dir,
                "icons/application",
                "application",
                APPLICATION_ICON_SIZES,
                "application.ico",
            ),
        ),
        (
            IDI_DEFAULT_PROCESS,
            named_icon_spec(
                assets_dir,
                "icons/default-process",
                "default-process",
                DEFAULT_PROCESS_ICON_SIZES,
                "default-process.ico",
            ),
        ),
    ];

    for (level, resource_id) in TRAY_ICON_IDS.iter().copied().enumerate() {
        let prefix = format!("cpu-usage-level-{level:02}");
        specs.push((
            resource_id,
            named_icon_spec(
                assets_dir,
                "icons/tray",
                &prefix,
                TRAY_ICON_SIZES,
                &format!("{prefix}.ico"),
            ),
        ));
    }
    specs
}

fn named_icon_spec(
    assets_dir: &Path,
    relative_dir: &str,
    prefix: &str,
    sizes: &[u32],
    output_name: &str,
) -> IconSpec {
    IconSpec {
        output_name: output_name.to_string(),
        sources: sizes
            .iter()
            .map(|size| IconSource {
                path: assets_dir
                    .join(relative_dir)
                    .join(format!("{prefix}-{size}.png")),
                size: *size,
            })
            .collect(),
    }
}

fn reject_source_ico_files(root: &Path) -> Result<(), String> {
    let entries = fs::read_dir(root).map_err(|error| {
        format!(
            "failed to inspect asset directory {}: {error}",
            root.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to inspect assets: {error}"))?;
        let path = entry.path();
        if path.is_dir() {
            reject_source_ico_files(&path)?;
        } else if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ico"))
        {
            return Err(format!(
                "source assets must not contain explicit ICO files: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_meter_bitmap(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read bitmap {}: {error}", path.display()))?;
    if bytes.len() < 30 || &bytes[0..2] != b"BM" {
        return Err(format!("invalid BMP header: {}", path.display()));
    }
    let width = i32::from_le_bytes(bytes[18..22].try_into().unwrap());
    let height = i32::from_le_bytes(bytes[22..26].try_into().unwrap());
    let bits_per_pixel = u16::from_le_bytes(bytes[28..30].try_into().unwrap());
    if width != 33 || height.unsigned_abs() != 75 || bits_per_pixel != 4 {
        return Err(format!(
            "meter bitmap {} is {width}x{height} at {bits_per_pixel} bpp; expected 33x75 at 4 bpp",
            path.display()
        ));
    }
    Ok(())
}

fn validate_resource_ids(
    icon_ids: impl Iterator<Item = u16>,
    bitmap_ids: impl Iterator<Item = u16>,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for resource_id in icon_ids.chain(bitmap_ids) {
        if !seen.insert(resource_id) {
            return Err(format!("duplicate embedded resource ID: {resource_id}"));
        }
    }
    Ok(())
}

fn rc_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if value.contains('"') {
        panic!("resource path contains a quote: {}", path.display());
    }
    value.replace('\\', "/")
}
