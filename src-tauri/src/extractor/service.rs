use super::metadata::parse_novelai_metadata;
use super::png_text::read_png_text_chunks;
use super::xlsx::{write_xlsx, WorkbookRow};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use unrar_ng::Archive;
use walkdir::WalkDir;
use zip::ZipArchive;

const THUMBNAIL_SIZE: u32 = 160;
const TEMP_ROOT: &str = r"D:\Agent\Agent_temp";
static RUN_TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize)]
pub struct FileWarning {
    pub path: String,
    pub message: String,
}

impl FileWarning {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub total_png: usize,
    pub processed: usize,
    pub failed: usize,
    pub output_path: String,
    pub warnings: Vec<FileWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressPayload {
    pub total_png: Option<usize>,
    pub processed: Option<usize>,
    pub failed: Option<usize>,
    pub current_file: Option<String>,
    pub message: Option<String>,
}

pub trait ProgressSink {
    fn emit_progress(&self, _event_name: &str, _payload: ProgressPayload) {}
    fn emit_warning(&self, _warning: &FileWarning) {}
}

#[cfg(test)]
struct NoopProgressSink;

#[cfg(test)]
impl ProgressSink for NoopProgressSink {}

#[derive(Debug, Clone)]
struct SourceImage {
    absolute_path: PathBuf,
    display_path: String,
}

struct RunTempDir {
    path: PathBuf,
}

impl RunTempDir {
    fn create() -> Result<Self> {
        let root = PathBuf::from(TEMP_ROOT).join("novelai_metadata_extractor");
        fs::create_dir_all(&root)
            .with_context(|| format!("无法创建临时目录：{}", root.display()))?;

        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let counter = RUN_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("run_{}_{}_{}", millis, std::process::id(), counter));
        fs::create_dir_all(&path)
            .with_context(|| format!("无法创建运行临时目录：{}", path.display()))?;

        Ok(Self { path })
    }
}

impl Drop for RunTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn run_extraction(
    input_path: &Path,
    output_path: &Path,
    progress: &dyn ProgressSink,
) -> Result<RunSummary> {
    validate_paths(input_path, output_path)?;

    progress.emit_progress(
        "extract:start",
        ProgressPayload {
            total_png: Some(0),
            processed: Some(0),
            failed: Some(0),
            current_file: None,
            message: Some("正在扫描输入路径...".to_string()),
        },
    );

    let temp_dir = RunTempDir::create()?;
    let prepared_input_path = prepare_input(input_path, &temp_dir.path)?;
    let images = collect_png_files(&prepared_input_path)?;
    let total_png = images.len();

    progress.emit_progress(
        "extract:scan_complete",
        ProgressPayload {
            total_png: Some(total_png),
            processed: Some(0),
            failed: Some(0),
            current_file: None,
            message: Some(format!("扫描完成，共找到 {} 个 PNG 文件。", total_png)),
        },
    );

    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    let mut failed = 0_usize;

    for (index, source) in images.iter().enumerate() {
        let mut file_failed = false;
        let mut metadata_warning = None;

        let metadata = match read_png_text_chunks(&source.absolute_path) {
            Ok(text_chunks) => {
                if text_chunks.is_empty() {
                    metadata_warning = Some("未找到 PNG 文本元数据。".to_string());
                    file_failed = true;
                }
                parse_novelai_metadata(&text_chunks)
            }
            Err(error) => {
                metadata_warning = Some(format!("无法读取 PNG 文本元数据：{error}"));
                file_failed = true;
                parse_novelai_metadata(&Default::default())
            }
        };

        if let Some(message) = metadata_warning {
            let warning = FileWarning::new(source.display_path.clone(), message);
            progress.emit_warning(&warning);
            warnings.push(warning);
        }

        match create_thumbnail(&source.absolute_path, &temp_dir.path, index) {
            Ok(thumbnail_path) => rows.push(WorkbookRow {
                thumbnail_path,
                source_path: source.display_path.clone(),
                positive_prompt: metadata.positive_prompt,
                negative_prompt: metadata.negative_prompt,
                artist_tags: metadata.artist_tags,
            }),
            Err(error) => {
                file_failed = true;
                let warning = FileWarning::new(
                    source.display_path.clone(),
                    format!("无法创建缩略图：{error}"),
                );
                progress.emit_warning(&warning);
                warnings.push(warning);
            }
        }

        if file_failed {
            failed += 1;
        }

        progress.emit_progress(
            "extract:file_progress",
            ProgressPayload {
                total_png: Some(total_png),
                processed: Some(index + 1),
                failed: Some(failed),
                current_file: Some(source.display_path.clone()),
                message: Some(format!("正在处理 {} / {}", index + 1, total_png)),
            },
        );
    }

    write_xlsx(&rows, output_path).context("无法生成 Excel 工作簿")?;

    progress.emit_progress(
        "extract:complete",
        ProgressPayload {
            total_png: Some(total_png),
            processed: Some(total_png),
            failed: Some(failed),
            current_file: None,
            message: Some("处理完成。".to_string()),
        },
    );

    Ok(RunSummary {
        total_png,
        processed: total_png,
        failed,
        output_path: output_path.display().to_string(),
        warnings,
    })
}

fn validate_paths(input_path: &Path, output_path: &Path) -> Result<()> {
    if !input_path.exists() {
        bail!("输入路径不存在。");
    }

    let output_extension = output_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !output_extension.eq_ignore_ascii_case("xlsx") {
        bail!("输出路径必须是 .xlsx 文件。");
    }

    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            bail!("输出目录不存在。");
        }
    }

    Ok(())
}

fn collect_png_files(input_path: &Path) -> Result<Vec<SourceImage>> {
    if input_path.is_file() {
        if is_png(input_path) {
            return Ok(vec![SourceImage {
                absolute_path: input_path.to_path_buf(),
                display_path: input_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| input_path.display().to_string()),
            }]);
        }

        if is_supported_archive(input_path) {
            bail!("压缩包输入未能解压。");
        }

        bail!("输入文件必须是 PNG、.zip、.7z 或 .rar。");
    }

    if !input_path.is_dir() {
        bail!("输入路径必须是文件夹、PNG 文件或受支持的压缩包。");
    }

    let mut images = Vec::new();
    for entry in WalkDir::new(input_path) {
        let entry = entry.with_context(|| format!("无法扫描目录：{}", input_path.display()))?;
        if !entry.file_type().is_file() || !is_png(entry.path()) {
            continue;
        }

        let display_path = entry
            .path()
            .strip_prefix(input_path)
            .unwrap_or_else(|_| entry.path())
            .display()
            .to_string();

        images.push(SourceImage {
            absolute_path: entry.path().to_path_buf(),
            display_path,
        });
    }

    images.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    Ok(images)
}

fn is_png(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
}

fn is_supported_archive(path: &Path) -> bool {
    archive_extension(path).is_some()
}

fn archive_extension(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_lowercase();
    match extension.as_str() {
        "zip" | "7z" | "rar" => Some(extension),
        _ => None,
    }
}

fn prepare_input(input_path: &Path, temp_dir: &Path) -> Result<PathBuf> {
    let Some(extension) = archive_extension(input_path) else {
        return Ok(input_path.to_path_buf());
    };

    let extract_dir = temp_dir.join("archive");
    fs::create_dir_all(&extract_dir)
        .with_context(|| format!("无法创建解压目录：{}", extract_dir.display()))?;

    match extension.as_str() {
        "zip" => extract_zip_archive(input_path, &extract_dir)?,
        "7z" => extract_7z_archive(input_path, &extract_dir)?,
        "rar" => extract_rar_archive(input_path, &extract_dir)?,
        _ => unreachable!(),
    }

    Ok(extract_dir)
}

fn extract_zip_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("无法打开 ZIP 压缩包：{}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file).context("无法读取 ZIP 压缩包")?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("无法读取 ZIP 条目：{index}"))?;
        let Some(enclosed_name) = entry.enclosed_name() else {
            continue;
        };
        let output_path = destination.join(enclosed_name);

        if entry.is_dir() {
            fs::create_dir_all(&output_path)
                .with_context(|| format!("无法创建目录：{}", output_path.display()))?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("无法创建目录：{}", parent.display()))?;
        }

        let mut output = File::create(&output_path)
            .with_context(|| format!("无法创建文件：{}", output_path.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("无法解压文件：{}", output_path.display()))?;
    }

    Ok(())
}

fn extract_7z_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    sevenz_rust::decompress_file(archive_path, destination)
        .with_context(|| format!("无法解压 7z 压缩包：{}", archive_path.display()))
}

fn extract_rar_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    Archive::new(archive_path)
        .open_for_processing()
        .with_context(|| format!("无法打开 RAR 压缩包：{}", archive_path.display()))?
        .extract_all(destination)
        .with_context(|| format!("无法解压 RAR 压缩包：{}", archive_path.display()))
}

fn create_thumbnail(source_path: &Path, temp_dir: &Path, index: usize) -> Result<PathBuf> {
    let thumbnails_dir = temp_dir.join("thumbnails");
    fs::create_dir_all(&thumbnails_dir)
        .with_context(|| format!("无法创建缩略图临时目录：{}", thumbnails_dir.display()))?;

    let image = image::open(source_path)
        .with_context(|| format!("无法读取图片：{}", source_path.display()))?;
    let thumbnail = image.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE);
    let thumbnail_path = thumbnails_dir.join(format!("{index:06}.png"));
    thumbnail
        .save_with_format(&thumbnail_path, image::ImageFormat::Png)
        .with_context(|| format!("无法保存缩略图：{}", thumbnail_path.display()))?;

    Ok(thumbnail_path)
}

#[cfg(test)]
mod tests {
    use super::{run_extraction, NoopProgressSink};
    use crc32fast::Hasher;
    use image::{Rgb, RgbImage};
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    #[test]
    fn exports_png_folder_to_xlsx() {
        let root = test_root("exports_png_folder_to_xlsx");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "best quality, artist:demo");
        insert_text_chunk(&png_path, "Comment", r#"{"uc":"bad hands"}"#);

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&input, &output, &NoopProgressSink).unwrap();

        assert_eq!(summary.total_png, 1);
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.failed, 0);
        assert!(summary.warnings.is_empty());
        assert!(output.exists());
        assert!(fs::metadata(&output).unwrap().len() > 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exports_zip_archive_to_xlsx() {
        let root = test_root("exports_zip_archive_to_xlsx");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:zip");

        let archive_path = root.join("images.zip");
        create_zip_archive(&archive_path, &png_path);

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&archive_path, &output, &NoopProgressSink).unwrap();

        assert_eq!(summary.total_png, 1);
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.failed, 0);
        assert!(output.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exports_7z_archive_to_xlsx() {
        let root = test_root("exports_7z_archive_to_xlsx");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:7z");

        let archive_path = root.join("images.7z");
        sevenz_rust::compress_to_path(&input, &archive_path).unwrap();

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&archive_path, &output, &NoopProgressSink).unwrap();

        assert_eq!(summary.total_png, 1);
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.failed, 0);
        assert!(output.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exports_rar_archive_to_xlsx() {
        let root = test_root("exports_rar_archive_to_xlsx");
        fs::create_dir_all(&root).unwrap();

        let archive_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("testfile.rar5-images.rar");
        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&archive_path, &output, &NoopProgressSink).unwrap();

        assert!(summary.total_png > 0);
        assert_eq!(summary.processed, summary.total_png);
        assert!(output.exists());

        fs::remove_dir_all(root).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        PathBuf::from(r"D:\Agent\Agent_temp")
            .join("novelai_metadata_extractor_tests")
            .join(format!("{name}_{millis}"))
    }

    fn create_test_png(path: &Path) {
        let image = RgbImage::from_pixel(4, 4, Rgb([40, 94, 172]));
        image.save(path).unwrap();
    }

    fn create_zip_archive(archive_path: &Path, png_path: &Path) {
        let file = File::create(archive_path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file("nested/sample.png", options).unwrap();
        zip.write_all(&fs::read(png_path).unwrap()).unwrap();
        zip.finish().unwrap();
    }

    fn insert_text_chunk(path: &Path, keyword: &str, text: &str) {
        let mut bytes = fs::read(path).unwrap();
        let iend_type_offset = bytes
            .windows(4)
            .rposition(|window| window == b"IEND")
            .unwrap();
        let iend_chunk_offset = iend_type_offset - 4;

        let mut data = Vec::new();
        data.extend(keyword.as_bytes());
        data.push(0);
        data.extend(text.as_bytes());

        let chunk = png_chunk(b"tEXt", &data);
        bytes.splice(iend_chunk_offset..iend_chunk_offset, chunk);
        fs::write(path, bytes).unwrap();
    }

    fn png_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend((data.len() as u32).to_be_bytes());
        output.extend(chunk_type);
        output.extend(data);

        let mut hasher = Hasher::new();
        hasher.update(chunk_type);
        hasher.update(data);
        output.extend(hasher.finalize().to_be_bytes());
        output
    }
}
