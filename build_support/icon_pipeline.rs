// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Windows 图标构建管线
//
//   文件:       build_support/icon_pipeline.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 将一组明确尺寸的 PNG 源图打包成临时 Windows ICO。
//!
//! 源图必须逐尺寸提供；此模块不会缩放、猜测或替换缺失图像。生成文件只写入
//! Cargo `OUT_DIR`，供资源编译器转换为最终的 `RT_GROUP_ICON`/`RT_ICON`。

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub(crate) struct IconSource {
    pub(crate) path: PathBuf,
    pub(crate) size: u32,
}

#[derive(Debug)]
pub(crate) struct IconSpec {
    pub(crate) output_name: String,
    pub(crate) sources: Vec<IconSource>,
}

pub(crate) fn generate_icon(out_dir: &Path, spec: &IconSpec) -> Result<PathBuf, String> {
    if spec.sources.is_empty() {
        return Err(format!("icon {} has no source images", spec.output_name));
    }
    if Path::new(&spec.output_name)
        .extension()
        .and_then(|value| value.to_str())
        != Some("ico")
    {
        return Err(format!(
            "generated icon name must use the .ico extension: {}",
            spec.output_name
        ));
    }

    let mut sources = spec.sources.clone();
    sources.sort_by_key(|source| source.size);
    let mut dimensions = BTreeSet::new();
    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);

    for source in sources {
        if !dimensions.insert(source.size) {
            return Err(format!(
                "icon {} declares size {} more than once",
                spec.output_name, source.size
            ));
        }
        if source.path.extension().and_then(|value| value.to_str()) != Some("png") {
            return Err(format!(
                "icon source must be PNG: {}",
                source.path.display()
            ));
        }

        let file = File::open(&source.path)
            .map_err(|error| format!("failed to open {}: {error}", source.path.display()))?;
        let image = ico::IconImage::read_png(BufReader::new(file))
            .map_err(|error| format!("invalid PNG {}: {error}", source.path.display()))?;
        if image.width() != source.size || image.height() != source.size {
            return Err(format!(
                "icon source {} is {}x{}, expected {}x{}",
                source.path.display(),
                image.width(),
                image.height(),
                source.size,
                source.size
            ));
        }

        let mut has_visible_pixel = false;
        let mut has_transparent_pixel = false;
        for pixel in image.rgba_data().chunks_exact(4) {
            has_visible_pixel |= pixel[3] != 0;
            has_transparent_pixel |= pixel[3] != u8::MAX;
        }
        if !has_visible_pixel || !has_transparent_pixel {
            return Err(format!(
                "icon source must contain visible and transparent pixels: {}",
                source.path.display()
            ));
        }

        let entry = ico::IconDirEntry::encode(&image)
            .map_err(|error| format!("failed to encode {}: {error}", source.path.display()))?;
        icon.add_entry(entry);
    }

    let output_path = out_dir.join(&spec.output_name);
    let output = File::create(&output_path)
        .map_err(|error| format!("failed to create {}: {error}", output_path.display()))?;
    icon.write(BufWriter::new(output))
        .map_err(|error| format!("failed to write {}: {error}", output_path.display()))?;
    Ok(output_path)
}
