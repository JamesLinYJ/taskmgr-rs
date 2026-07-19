// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Windows 图像资源管线测试
//
//   文件:       tests/resource_assets.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 验证仓库图像资产和构建期 ICO 的确定性输入约束。

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "../build_support/icon_pipeline.rs"]
mod icon_pipeline;
#[allow(dead_code)]
#[path = "../src/ui/resource_ids.rs"]
mod resource_ids;

use icon_pipeline::{IconSource, IconSpec, generate_icon};
use resource_ids::{
    IDB_METER_LIT_GREEN, IDB_METER_LIT_RED, IDB_METER_UNLIT, IDI_APPLICATION, IDI_DEFAULT_PROCESS,
    IDI_TRAY_CPU_00, IDI_TRAY_CPU_01, IDI_TRAY_CPU_02, IDI_TRAY_CPU_03, IDI_TRAY_CPU_04,
    IDI_TRAY_CPU_05, IDI_TRAY_CPU_06, IDI_TRAY_CPU_07, IDI_TRAY_CPU_08, IDI_TRAY_CPU_09,
    IDI_TRAY_CPU_10, IDI_TRAY_CPU_11,
};

const APPLICATION_SIZES: &[u32] = &[16, 20, 24, 32, 40, 48, 64, 256];
const DEFAULT_PROCESS_SIZES: &[u32] = &[16, 20, 24, 32, 40, 48, 64];
const TRAY_SIZES: &[u32] = &[16, 20, 24, 32];

#[test]
fn source_assets_contain_no_explicit_ico_files() {
    let mut ico_files = Vec::new();
    collect_files_with_extension(&asset_root(), "ico", &mut ico_files);
    assert!(
        ico_files.is_empty(),
        "unexpected ICO sources: {ico_files:?}"
    );
}

#[test]
fn source_asset_manifest_contains_only_declared_files() {
    let root = asset_root();
    let mut expected = BTreeSet::new();
    for spec in all_icon_specs() {
        expected.extend(spec.sources.into_iter().map(|source| source.path));
    }
    expected.extend([
        root.join("bitmaps/meter-segment-lit-green.bmp"),
        root.join("bitmaps/meter-segment-lit-red.bmp"),
        root.join("bitmaps/meter-segment-unlit.bmp"),
        root.join("windows/taskmgr.manifest"),
    ]);

    let mut actual = BTreeSet::new();
    collect_all_files(&root, &mut actual);
    assert_eq!(actual, expected);
}

#[test]
fn embedded_image_resource_ids_are_unique() {
    let ids = [
        IDI_APPLICATION,
        IDI_DEFAULT_PROCESS,
        IDI_TRAY_CPU_00,
        IDI_TRAY_CPU_01,
        IDI_TRAY_CPU_02,
        IDI_TRAY_CPU_03,
        IDI_TRAY_CPU_04,
        IDI_TRAY_CPU_05,
        IDI_TRAY_CPU_06,
        IDI_TRAY_CPU_07,
        IDI_TRAY_CPU_08,
        IDI_TRAY_CPU_09,
        IDI_TRAY_CPU_10,
        IDI_TRAY_CPU_11,
        IDB_METER_LIT_GREEN,
        IDB_METER_LIT_RED,
        IDB_METER_UNLIT,
    ];
    assert_eq!(ids.into_iter().collect::<HashSet<_>>().len(), ids.len());
}

#[test]
fn meter_bitmaps_keep_the_declared_classic_format() {
    for name in [
        "meter-segment-lit-green.bmp",
        "meter-segment-lit-red.bmp",
        "meter-segment-unlit.bmp",
    ] {
        let path = asset_root().join("bitmaps").join(name);
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        assert!(bytes.len() >= 30, "truncated BMP: {}", path.display());
        assert_eq!(&bytes[0..2], b"BM", "invalid BMP: {}", path.display());
        assert_eq!(i32::from_le_bytes(bytes[18..22].try_into().unwrap()), 33);
        assert_eq!(
            i32::from_le_bytes(bytes[22..26].try_into().unwrap()).unsigned_abs(),
            75
        );
        assert_eq!(u16::from_le_bytes(bytes[28..30].try_into().unwrap()), 4);
    }
}

#[test]
fn every_declared_icon_source_has_exact_dimensions_and_alpha() {
    for spec in all_icon_specs() {
        for source in spec.sources {
            let file = File::open(&source.path).unwrap_or_else(|error| {
                panic!("failed to open {}: {error}", source.path.display())
            });
            let image = ico::IconImage::read_png(BufReader::new(file))
                .unwrap_or_else(|error| panic!("invalid PNG {}: {error}", source.path.display()));
            assert_eq!((image.width(), image.height()), (source.size, source.size));
            assert!(
                image.rgba_data().chunks_exact(4).any(|pixel| pixel[3] != 0),
                "{} has no visible pixels",
                source.path.display()
            );
            assert!(
                image
                    .rgba_data()
                    .chunks_exact(4)
                    .any(|pixel| pixel[3] != u8::MAX),
                "{} has no transparent pixels",
                source.path.display()
            );
        }
    }
}

#[test]
fn generated_ico_contains_one_entry_for_each_declared_size() {
    let out_dir = unique_test_directory();
    fs::create_dir_all(&out_dir).expect("temporary icon directory should be created");

    for spec in all_icon_specs() {
        let expected_sizes: Vec<_> = spec.sources.iter().map(|source| source.size).collect();
        let output = generate_icon(&out_dir, &spec).expect("declared icon should generate");
        let icon = ico::IconDir::read(BufReader::new(
            File::open(output).expect("generated ICO should open"),
        ))
        .expect("generated ICO should parse");
        let actual_sizes: Vec<_> = icon.entries().iter().map(|entry| entry.width()).collect();
        assert_eq!(actual_sizes, expected_sizes);
    }

    fs::remove_dir_all(out_dir).expect("temporary icon directory should be removed");
}

fn all_icon_specs() -> Vec<IconSpec> {
    let root = asset_root();
    let mut specs = vec![
        icon_spec(&root, "application", "application", APPLICATION_SIZES),
        icon_spec(
            &root,
            "default-process",
            "default-process",
            DEFAULT_PROCESS_SIZES,
        ),
    ];
    for level in 0..12 {
        let prefix = format!("cpu-usage-level-{level:02}");
        specs.push(icon_spec(&root, "tray", &prefix, TRAY_SIZES));
    }
    specs
}

fn icon_spec(root: &Path, directory: &str, prefix: &str, sizes: &[u32]) -> IconSpec {
    IconSpec {
        output_name: format!("{prefix}.ico"),
        sources: sizes
            .iter()
            .map(|size| IconSource {
                path: root
                    .join("icons")
                    .join(directory)
                    .join(format!("{prefix}-{size}.png")),
                size: *size,
            })
            .collect(),
    }
}

fn asset_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
}

fn unique_test_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should follow the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "taskmgr-rs-icon-test-{}-{nonce}",
        std::process::id()
    ))
}

fn collect_files_with_extension(root: &Path, extension: &str, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()))
    {
        let path = entry
            .expect("asset directory entry should be readable")
            .path();
        if path.is_dir() {
            collect_files_with_extension(&path, extension, files);
        } else if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            files.push(path);
        }
    }
}

fn collect_all_files(root: &Path, files: &mut BTreeSet<PathBuf>) {
    for entry in fs::read_dir(root)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()))
    {
        let path = entry
            .expect("asset directory entry should be readable")
            .path();
        if path.is_dir() {
            collect_all_files(&path, files);
        } else {
            files.insert(path);
        }
    }
}
