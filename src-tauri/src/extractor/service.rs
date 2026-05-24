use super::metadata::parse_novelai_metadata;
use super::png_text::read_png_text_chunks;
use super::xlsx::{write_xlsx, WorkbookRow};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::HashMap;
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
    pub skipped_duplicates: usize,
    pub output_path: String,
    pub warnings: Vec<FileWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressPayload {
    pub total_png: Option<usize>,
    pub processed: Option<usize>,
    pub failed: Option<usize>,
    pub skipped_duplicates: Option<usize>,
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

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractionOptions {
    pub dedupe_positive_prompt: bool,
    pub dedupe_artist_tags: bool,
}

#[derive(Debug, Clone, Copy)]
struct DuplicateMatch {
    group_index: usize,
    reason: &'static str,
}

#[derive(Debug, Clone)]
struct DuplicateGroup {
    row_index: usize,
    representative_path: PathBuf,
    representative_display_path: String,
    folder_name: Option<String>,
    copied_count: usize,
}

struct DuplicateFolderWriter {
    output_dir: PathBuf,
    next_folder_number: usize,
}

struct FailureFolderWriter {
    output_dir: PathBuf,
    folder_path: Option<PathBuf>,
    copied_count: usize,
}

impl DuplicateFolderWriter {
    fn new(output_path: &Path) -> Self {
        Self {
            output_dir: output_directory(output_path),
            next_folder_number: 1,
        }
    }

    fn ensure_folder(
        &mut self,
        group: &mut DuplicateGroup,
        rows: &mut [WorkbookRow],
    ) -> Result<PathBuf> {
        if let Some(folder_name) = &group.folder_name {
            return Ok(self.output_dir.join(folder_name));
        }

        let (folder_name, folder_path) = self.create_folder()?;
        copy_source_to_numbered_folder(
            &group.representative_path,
            &group.representative_display_path,
            &folder_path,
            &mut group.copied_count,
            "重复图片",
        )?;

        rows[group.row_index].duplicate_folder = format!("{folder_name}/");
        group.folder_name = Some(folder_name);

        Ok(folder_path)
    }

    fn create_folder(&mut self) -> Result<(String, PathBuf)> {
        loop {
            let folder_name = format!("image{}", self.next_folder_number);
            self.next_folder_number += 1;
            let folder_path = self.output_dir.join(&folder_name);

            if folder_path.exists() {
                continue;
            }

            fs::create_dir(&folder_path)
                .with_context(|| format!("无法创建重复图片文件夹：{}", folder_path.display()))?;
            return Ok((folder_name, folder_path));
        }
    }
}

impl FailureFolderWriter {
    fn new(output_path: &Path) -> Self {
        Self {
            output_dir: output_directory(output_path),
            folder_path: None,
            copied_count: 0,
        }
    }

    fn copy_failed_source(&mut self, source: &SourceImage) -> Result<()> {
        let folder_path = self.ensure_folder()?;
        copy_source_to_numbered_folder(
            &source.absolute_path,
            &source.display_path,
            &folder_path,
            &mut self.copied_count,
            "失败图片",
        )
    }

    fn ensure_folder(&mut self) -> Result<PathBuf> {
        if let Some(folder_path) = &self.folder_path {
            return Ok(folder_path.clone());
        }

        let folder_path = self.output_dir.join("_Fail");
        fs::create_dir(&folder_path)
            .with_context(|| format!("无法创建失败图片文件夹：{}", folder_path.display()))?;
        self.folder_path = Some(folder_path.clone());

        Ok(folder_path)
    }
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

#[cfg(test)]
fn run_extraction(
    input_path: &Path,
    output_path: &Path,
    progress: &dyn ProgressSink,
) -> Result<RunSummary> {
    run_extraction_with_options(
        input_path,
        output_path,
        ExtractionOptions::default(),
        progress,
    )
}

pub fn run_extraction_with_options(
    input_path: &Path,
    output_path: &Path,
    options: ExtractionOptions,
    progress: &dyn ProgressSink,
) -> Result<RunSummary> {
    validate_paths(input_path, output_path)?;

    progress.emit_progress(
        "extract:start",
        ProgressPayload {
            total_png: Some(0),
            processed: Some(0),
            failed: Some(0),
            skipped_duplicates: Some(0),
            current_file: None,
            message: Some("正在扫描输入路径...".to_string()),
        },
    );

    let temp_dir = RunTempDir::create()?;
    let prepared_input_path = prepare_input(input_path, &temp_dir.path)?;
    let images = collect_png_files(&prepared_input_path)?;
    let total_png = images.len();
    let output_path = prepare_output_package(output_path)?;

    progress.emit_progress(
        "extract:scan_complete",
        ProgressPayload {
            total_png: Some(total_png),
            processed: Some(0),
            failed: Some(0),
            skipped_duplicates: Some(0),
            current_file: None,
            message: Some(format!("扫描完成，共找到 {} 个 PNG 文件。", total_png)),
        },
    );

    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    let mut failed = 0_usize;
    let mut skipped_duplicates = 0_usize;
    let mut positive_prompt_groups = HashMap::new();
    let mut artist_string_groups = HashMap::new();
    let mut duplicate_groups = Vec::new();
    let mut duplicate_folder_writer = DuplicateFolderWriter::new(&output_path);
    let mut failure_folder_writer = FailureFolderWriter::new(&output_path);

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

        if let Some(duplicate) = duplicate_match(
            &metadata.positive_prompt,
            &metadata.artist_tags,
            options,
            &positive_prompt_groups,
            &artist_string_groups,
        ) {
            skipped_duplicates += 1;

            let folder_path = {
                let group = &mut duplicate_groups[duplicate.group_index];
                duplicate_folder_writer.ensure_folder(group, &mut rows)?
            };

            {
                let group = &mut duplicate_groups[duplicate.group_index];
                copy_source_to_numbered_folder(
                    &source.absolute_path,
                    &source.display_path,
                    &folder_path,
                    &mut group.copied_count,
                    "重复图片",
                )?;
            }

            if file_failed {
                failure_folder_writer.copy_failed_source(source)?;
                failed += 1;
            }

            progress.emit_progress(
                "extract:file_progress",
                ProgressPayload {
                    total_png: Some(total_png),
                    processed: Some(index + 1),
                    failed: Some(failed),
                    skipped_duplicates: Some(skipped_duplicates),
                    current_file: Some(source.display_path.clone()),
                    message: Some(format!(
                        "正在处理 {} / {}，已去重跳过 {} 张（{}）",
                        index + 1,
                        total_png,
                        skipped_duplicates,
                        duplicate.reason
                    )),
                },
            );
            continue;
        }

        match create_thumbnail(&source.absolute_path, &temp_dir.path, index) {
            Ok(thumbnail_path) => {
                let row_index = rows.len();
                remember_dedup_group(
                    source,
                    row_index,
                    &metadata.positive_prompt,
                    &metadata.artist_tags,
                    options,
                    &mut duplicate_groups,
                    &mut positive_prompt_groups,
                    &mut artist_string_groups,
                );
                rows.push(WorkbookRow {
                    thumbnail_path,
                    source_path: source.display_path.clone(),
                    positive_prompt: metadata.positive_prompt,
                    negative_prompt: metadata.negative_prompt,
                    artist_tags: metadata.artist_tags,
                    duplicate_folder: String::new(),
                });
            }
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
            failure_folder_writer.copy_failed_source(source)?;
            failed += 1;
        }

        progress.emit_progress(
            "extract:file_progress",
            ProgressPayload {
                total_png: Some(total_png),
                processed: Some(index + 1),
                failed: Some(failed),
                skipped_duplicates: Some(skipped_duplicates),
                current_file: Some(source.display_path.clone()),
                message: Some(format!("正在处理 {} / {}", index + 1, total_png)),
            },
        );
    }

    write_xlsx(&rows, &output_path).context("无法生成 Excel 工作簿")?;

    progress.emit_progress(
        "extract:complete",
        ProgressPayload {
            total_png: Some(total_png),
            processed: Some(total_png),
            failed: Some(failed),
            skipped_duplicates: Some(skipped_duplicates),
            current_file: None,
            message: Some("处理完成。".to_string()),
        },
    );

    Ok(RunSummary {
        total_png,
        processed: total_png,
        failed,
        skipped_duplicates,
        output_path: output_path.display().to_string(),
        warnings,
    })
}

fn duplicate_match(
    positive_prompt: &str,
    artist_tags: &[String],
    options: ExtractionOptions,
    positive_prompt_groups: &HashMap<String, usize>,
    artist_string_groups: &HashMap<String, usize>,
) -> Option<DuplicateMatch> {
    if options.dedupe_positive_prompt {
        if let Some(key) = positive_prompt_key(positive_prompt) {
            if let Some(group_index) = positive_prompt_groups.get(&key) {
                return Some(DuplicateMatch {
                    group_index: *group_index,
                    reason: "正面提示词重复",
                });
            }
        }
    }

    if options.dedupe_artist_tags {
        if let Some(key) = artist_tags_key(artist_tags) {
            if let Some(group_index) = artist_string_groups.get(&key) {
                return Some(DuplicateMatch {
                    group_index: *group_index,
                    reason: "画师串重复",
                });
            }
        }
    }

    None
}

fn remember_dedup_group(
    source: &SourceImage,
    row_index: usize,
    positive_prompt: &str,
    artist_tags: &[String],
    options: ExtractionOptions,
    duplicate_groups: &mut Vec<DuplicateGroup>,
    positive_prompt_groups: &mut HashMap<String, usize>,
    artist_string_groups: &mut HashMap<String, usize>,
) {
    let positive_key = options
        .dedupe_positive_prompt
        .then(|| positive_prompt_key(positive_prompt))
        .flatten();
    let artist_key = options
        .dedupe_artist_tags
        .then(|| artist_tags_key(artist_tags))
        .flatten();

    if positive_key.is_none() && artist_key.is_none() {
        return;
    }

    let group_index = duplicate_groups.len();
    duplicate_groups.push(DuplicateGroup {
        row_index,
        representative_path: source.absolute_path.clone(),
        representative_display_path: source.display_path.clone(),
        folder_name: None,
        copied_count: 0,
    });

    if options.dedupe_positive_prompt {
        if let Some(key) = positive_key {
            positive_prompt_groups.insert(key, group_index);
        }
    }

    if options.dedupe_artist_tags {
        if let Some(key) = artist_key {
            artist_string_groups.insert(key, group_index);
        }
    }
}

fn positive_prompt_key(value: &str) -> Option<String> {
    non_empty_key(value)
}

fn artist_tags_key(values: &[String]) -> Option<String> {
    let key = values
        .iter()
        .filter_map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .collect::<Vec<_>>()
        .join("\n");
    non_empty_key(&key)
}

fn non_empty_key(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn output_directory(output_path: &Path) -> PathBuf {
    output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn prepare_output_package(requested_output_path: &Path) -> Result<PathBuf> {
    let parent_dir = output_directory(requested_output_path);
    let folder_name = output_package_folder_name(requested_output_path);
    let output_dir = create_unique_output_dir(&parent_dir, &folder_name)?;
    let file_name = requested_output_path
        .file_name()
        .map(|value| sanitize_file_name(&value.to_string_lossy()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "novelai_metadata.xlsx".to_string());

    Ok(output_dir.join(file_name))
}

fn output_package_folder_name(requested_output_path: &Path) -> String {
    let folder_name = requested_output_path
        .file_stem()
        .map(|value| sanitize_file_name(&value.to_string_lossy()))
        .unwrap_or_else(|| "novelai_metadata".to_string());
    let folder_name = folder_name
        .trim_matches(|character| character == ' ' || character == '.')
        .to_string();

    if folder_name.is_empty() {
        "novelai_metadata".to_string()
    } else {
        folder_name
    }
}

fn create_unique_output_dir(parent_dir: &Path, folder_name: &str) -> Result<PathBuf> {
    for index in 0_usize.. {
        let candidate_name = if index == 0 {
            folder_name.to_string()
        } else {
            format!("{folder_name}_{index}")
        };
        let candidate_path = parent_dir.join(candidate_name);

        match fs::create_dir(&candidate_path) {
            Ok(()) => return Ok(candidate_path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("无法创建输出文件夹：{}", candidate_path.display()));
            }
        }
    }

    unreachable!("unbounded output folder numbering should always find a candidate")
}

fn copy_source_to_numbered_folder(
    source_path: &Path,
    display_path: &str,
    folder_path: &Path,
    copied_count: &mut usize,
    item_label: &str,
) -> Result<()> {
    *copied_count += 1;

    let source_file_name = source_path
        .file_name()
        .map(|value| sanitize_file_name(&value.to_string_lossy()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "image.png".to_string());
    let target_path = folder_path.join(format!("{:04}_{source_file_name}", *copied_count));

    fs::copy(source_path, &target_path).with_context(|| {
        format!(
            "无法复制{} {} 到 {}",
            item_label,
            display_path,
            target_path.display()
        )
    })?;

    Ok(())
}

fn sanitize_file_name(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ if character.is_control() => '_',
            _ => character,
        })
        .collect()
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
    use super::{
        run_extraction, run_extraction_with_options, ExtractionOptions, NoopProgressSink,
        RunSummary,
    };
    use crc32fast::Hasher;
    use image::{Rgb, RgbImage};
    use std::fs;
    use std::fs::File;
    use std::io::{Read, Write};
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
        let actual_output = actual_output_path(&summary);
        assert_eq!(actual_output, root.join("metadata").join("metadata.xlsx"));
        assert!(!output.exists());
        assert!(actual_output.exists());
        assert!(fs::metadata(&actual_output).unwrap().len() > 0);
        assert_xlsx_contains(&actual_output, &["best quality, artist:demo", "bad hands"]);
        assert!(!root.join("image1").exists());
        assert!(!actual_output.parent().unwrap().join("_Fail").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creates_unique_output_package_folder_when_name_exists() {
        let root = test_root("creates_unique_output_package_folder_when_name_exists");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(root.join("metadata")).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:package");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&input, &output, &NoopProgressSink).unwrap();
        let actual_output = actual_output_path(&summary);

        assert_eq!(actual_output, root.join("metadata_1").join("metadata.xlsx"));
        assert!(!output.exists());
        assert!(actual_output.exists());
        assert_xlsx_contains(&actual_output, &["artist:package"]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scans_nested_png_folder() {
        let root = test_root("scans_nested_png_folder");
        let input = root.join("input").join("nested");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:nested");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&root.join("input"), &output, &NoopProgressSink).unwrap();

        assert_eq!(summary.total_png, 1);
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.failed, 0);
        let actual_output = actual_output_path(&summary);
        assert_eq!(actual_output, root.join("metadata").join("metadata.xlsx"));
        assert!(!output.exists());
        assert_xlsx_contains(&actual_output, &["artist:nested"]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deduplicates_by_positive_prompt_when_enabled() {
        let root = test_root("deduplicates_by_positive_prompt_when_enabled");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "same prompt, artist:first");

        let second_png = input.join("second.png");
        create_test_png(&second_png);
        insert_text_chunk(&second_png, "Description", "same prompt, artist:first");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                dedupe_positive_prompt: true,
                dedupe_artist_tags: false,
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 2);
        assert_eq!(summary.processed, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped_duplicates, 1);
        let actual_output = actual_output_path(&summary);
        let actual_output_dir = actual_output.parent().unwrap();
        assert!(!output.exists());
        assert_xlsx_media_count(&actual_output, 1);
        assert_xlsx_contains(&actual_output, &["same prompt, artist:first", "image1/"]);
        assert_duplicate_folder_contains(
            &actual_output_dir.join("image1"),
            &["first.png", "second.png"],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deduplicates_by_artist_string_when_enabled() {
        let root = test_root("deduplicates_by_artist_string_when_enabled");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "first prompt, artist:same");

        let second_png = input.join("second.png");
        create_test_png(&second_png);
        insert_text_chunk(&second_png, "Description", "second prompt, artist:same");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                dedupe_positive_prompt: false,
                dedupe_artist_tags: true,
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 2);
        assert_eq!(summary.processed, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped_duplicates, 1);
        let actual_output = actual_output_path(&summary);
        let actual_output_dir = actual_output.parent().unwrap();
        assert!(!output.exists());
        assert_xlsx_media_count(&actual_output, 1);
        assert_xlsx_contains(&actual_output, &["first prompt, artist:same", "image1/"]);
        assert_duplicate_folder_contains(
            &actual_output_dir.join("image1"),
            &["first.png", "second.png"],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creates_numbered_duplicate_folders_per_duplicate_group() {
        let root = test_root("creates_numbered_duplicate_folders_per_duplicate_group");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "prompt A, artist:first");

        let second_png = input.join("second.png");
        create_test_png(&second_png);
        insert_text_chunk(&second_png, "Description", "prompt A, artist:first");

        let third_png = input.join("third.png");
        create_test_png(&third_png);
        insert_text_chunk(&third_png, "Description", "prompt B, artist:second");

        let fourth_png = input.join("fourth.png");
        create_test_png(&fourth_png);
        insert_text_chunk(&fourth_png, "Description", "prompt B, artist:second");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                dedupe_positive_prompt: true,
                dedupe_artist_tags: false,
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 4);
        assert_eq!(summary.processed, 4);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped_duplicates, 2);
        let actual_output = actual_output_path(&summary);
        let actual_output_dir = actual_output.parent().unwrap();
        assert!(!output.exists());
        assert_xlsx_contains(
            &actual_output,
            &[
                "prompt A, artist:first",
                "prompt B, artist:second",
                "image1/",
                "image2/",
            ],
        );
        assert_duplicate_folder_contains(
            &actual_output_dir.join("image1"),
            &["first.png", "second.png"],
        );
        assert_duplicate_folder_contains(
            &actual_output_dir.join("image2"),
            &["third.png", "fourth.png"],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn handles_empty_folder() {
        let root = test_root("handles_empty_folder");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&input, &output, &NoopProgressSink).unwrap();

        assert_eq!(summary.total_png, 0);
        assert_eq!(summary.processed, 0);
        assert_eq!(summary.failed, 0);
        assert!(summary.warnings.is_empty());
        let actual_output = actual_output_path(&summary);
        assert!(!output.exists());
        assert!(actual_output.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_non_novelai_png_without_crashing() {
        let root = test_root("reports_non_novelai_png_without_crashing");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        create_test_png(&input.join("plain.png"));

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&input, &output, &NoopProgressSink).unwrap();

        assert_eq!(summary.total_png, 1);
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.warnings.len(), 1);
        let actual_output = actual_output_path(&summary);
        assert!(!output.exists());
        assert!(actual_output.exists());
        assert_xlsx_has_media(&actual_output);
        assert_duplicate_folder_contains(
            &actual_output.parent().unwrap().join("_Fail"),
            &["plain.png"],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_broken_png_without_crashing() {
        let root = test_root("reports_broken_png_without_crashing");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        fs::write(input.join("broken.png"), b"not a png").unwrap();

        let output = root.join("metadata.xlsx");
        let summary = run_extraction(&input, &output, &NoopProgressSink).unwrap();

        assert_eq!(summary.total_png, 1);
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.failed, 1);
        assert!(!summary.warnings.is_empty());
        let actual_output = actual_output_path(&summary);
        assert!(!output.exists());
        assert!(actual_output.exists());
        assert_duplicate_folder_contains(
            &actual_output.parent().unwrap().join("_Fail"),
            &["broken.png"],
        );

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
        let actual_output = actual_output_path(&summary);
        assert!(!output.exists());
        assert!(actual_output.exists());

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
        let actual_output = actual_output_path(&summary);
        assert!(!output.exists());
        assert!(actual_output.exists());

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
        let actual_output = actual_output_path(&summary);
        assert!(!output.exists());
        assert!(actual_output.exists());

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

    fn actual_output_path(summary: &RunSummary) -> PathBuf {
        PathBuf::from(&summary.output_path)
    }

    fn assert_xlsx_contains(output: &Path, expected_text: &[&str]) {
        let file = File::open(output).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name.starts_with("xl/media/")));

        let mut shared_strings = String::new();
        archive
            .by_name("xl/sharedStrings.xml")
            .unwrap()
            .read_to_string(&mut shared_strings)
            .unwrap();

        for text in expected_text {
            assert!(shared_strings.contains(text));
        }
    }

    fn assert_xlsx_has_media(output: &Path) {
        let file = File::open(output).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name.starts_with("xl/media/")));
    }

    fn assert_xlsx_media_count(output: &Path, expected_count: usize) {
        let file = File::open(output).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let media_count = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .filter(|name| name.starts_with("xl/media/"))
            .count();

        assert_eq!(media_count, expected_count);
    }

    fn assert_duplicate_folder_contains(folder: &Path, expected_file_names: &[&str]) {
        assert!(
            folder.is_dir(),
            "{} should be a directory",
            folder.display()
        );

        let copied_files = fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(copied_files.len(), expected_file_names.len());
        for expected_file_name in expected_file_names {
            assert!(
                copied_files
                    .iter()
                    .any(|copied_file| copied_file.ends_with(expected_file_name)),
                "{} should contain a copied {} entry, got {:?}",
                folder.display(),
                expected_file_name,
                copied_files
            );
        }
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
