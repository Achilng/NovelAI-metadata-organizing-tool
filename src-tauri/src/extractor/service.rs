use super::cache::{CacheStore, CachedImageRecord, FileFingerprint};
use super::metadata::{parse_novelai_metadata, NovelAiMetadata};
use super::png_text::read_png_text_chunks;
use super::xlsx::{write_xlsx, WorkbookRow};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::Serialize;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use unrar_ng::Archive;
use walkdir::WalkDir;
use zip::ZipArchive;

const THUMBNAIL_SIZE: u32 = 160;
const TEMP_ROOT: &str = r"D:\Agent\Agent_temp";
const MISSING_TIME_FOLDER_PREFIX: &str = "9999-12-31_235959";
const MAX_IMAGE_WORKER_THREADS: usize = 8;
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
    pub cache_hits: usize,
    pub processed_new: usize,
    pub output_path: String,
    pub warnings: Vec<FileWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressPayload {
    pub total_png: Option<usize>,
    pub processed: Option<usize>,
    pub failed: Option<usize>,
    pub skipped_duplicates: Option<usize>,
    pub cache_hits: Option<usize>,
    pub processed_new: Option<usize>,
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
    fingerprint: FileFingerprint,
    sort_time: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractionOptions {
    pub dedupe_positive_prompt: bool,
    pub dedupe_artist_tags: bool,
    pub sort_by_time: bool,
    pub incremental: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheMatchMode {
    FileSystem,
    Archive,
}

struct PreparedInput {
    path: PathBuf,
    cache_match_mode: CacheMatchMode,
    archive_fingerprint: Option<FileFingerprint>,
}

#[derive(Debug, Clone, Copy)]
struct DuplicateMatch {
    group_index: usize,
    reason: &'static str,
}

#[derive(Debug, Clone)]
struct ImageFolderGroup {
    row_index: usize,
    representative_path: PathBuf,
    representative_display_path: String,
    duplicate_sources: Vec<SourceImage>,
    sort_time: Option<SystemTime>,
    copied_count: usize,
}

struct ImageFolderWriter {
    output_dir: PathBuf,
    next_folder_number: usize,
}

struct FailureFolderWriter {
    output_dir: PathBuf,
    failed_sources: Vec<SourceImage>,
    sort_time: Option<SystemTime>,
    copied_count: usize,
}

impl ImageFolderWriter {
    fn new(output_path: &Path) -> Self {
        Self {
            output_dir: output_directory(output_path),
            next_folder_number: 1,
        }
    }

    fn write_image_folders(
        &mut self,
        groups: &mut [ImageFolderGroup],
        rows: &mut [WorkbookRow],
        sort_by_time: bool,
    ) -> Result<()> {
        for group in groups.iter_mut() {
            let (folder_name, folder_path) = self.create_folder(group.sort_time, sort_by_time)?;
            copy_source_to_numbered_folder(
                &group.representative_path,
                &group.representative_display_path,
                &folder_path,
                &mut group.copied_count,
                "图片",
            )?;

            for source in &group.duplicate_sources {
                copy_source_to_numbered_folder(
                    &source.absolute_path,
                    &source.display_path,
                    &folder_path,
                    &mut group.copied_count,
                    "图片",
                )?;
            }

            rows[group.row_index].image_folder = format!("{folder_name}/");
        }

        Ok(())
    }

    fn create_folder(
        &mut self,
        sort_time: Option<SystemTime>,
        sort_by_time: bool,
    ) -> Result<(String, PathBuf)> {
        loop {
            let base_folder_name = format!("image{}", self.next_folder_number);
            self.next_folder_number += 1;
            let folder_name = output_folder_name(&base_folder_name, sort_time, sort_by_time);
            let folder_path = self.output_dir.join(&folder_name);

            if folder_path.exists() {
                continue;
            }

            fs::create_dir(&folder_path)
                .with_context(|| format!("无法创建图片文件夹：{}", folder_path.display()))?;
            return Ok((folder_name, folder_path));
        }
    }
}

impl FailureFolderWriter {
    fn new(output_path: &Path) -> Self {
        Self {
            output_dir: output_directory(output_path),
            failed_sources: Vec::new(),
            sort_time: None,
            copied_count: 0,
        }
    }

    fn remember_failed_source(&mut self, source: &SourceImage) {
        self.sort_time = earliest_sort_time(self.sort_time, source.sort_time);
        self.failed_sources.push(source.clone());
    }

    fn write_failed_sources(&mut self, sort_by_time: bool) -> Result<()> {
        if self.failed_sources.is_empty() {
            return Ok(());
        }

        let base_folder_name = if sort_by_time { "Fail" } else { "_Fail" };
        let folder_name = output_folder_name(base_folder_name, self.sort_time, sort_by_time);
        let folder_path = self.output_dir.join(folder_name);
        fs::create_dir(&folder_path)
            .with_context(|| format!("无法创建失败图片文件夹：{}", folder_path.display()))?;

        for source in &self.failed_sources {
            copy_source_to_numbered_folder(
                &source.absolute_path,
                &source.display_path,
                &folder_path,
                &mut self.copied_count,
                "失败图片",
            )?;
        }

        Ok(())
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
            cache_hits: Some(0),
            processed_new: Some(0),
            current_file: None,
            message: Some("正在扫描输入路径...".to_string()),
        },
    );

    let temp_dir = RunTempDir::create()?;
    let prepared_input = prepare_input(input_path, &temp_dir.path)?;
    let images = collect_png_files(&prepared_input.path)?;
    let total_png = images.len();
    let worker_count = image_worker_count(total_png);
    let worker_count_description = worker_count_description(worker_count);
    let mut cache = if options.incremental {
        Some(CacheStore::open(
            &output_directory(output_path),
            input_path,
            prepared_input.archive_fingerprint,
        )?)
    } else {
        None
    };
    let existing_cache_records = cache.as_ref().map(CacheStore::record_count).unwrap_or(0);
    let output_path = prepare_output_package(output_path)?;

    progress.emit_progress(
        "extract:scan_complete",
        ProgressPayload {
            total_png: Some(total_png),
            processed: Some(0),
            failed: Some(0),
            skipped_duplicates: Some(0),
            cache_hits: Some(0),
            processed_new: Some(0),
            current_file: None,
            message: Some(if options.incremental {
                format!(
                    "扫描完成，共找到 {} 个 PNG 文件，已加载 {} 条缓存记录，{}。",
                    total_png, existing_cache_records, worker_count_description
                )
            } else {
                format!(
                    "扫描完成，共找到 {} 个 PNG 文件，{}。",
                    total_png, worker_count_description
                )
            }),
        },
    );

    let mut rows: Vec<WorkbookRow> = Vec::new();
    let mut warnings = Vec::new();
    let mut failed = 0_usize;
    let mut skipped_duplicates = 0_usize;
    let mut cache_hits = 0_usize;
    let mut processed_new = 0_usize;
    let mut positive_prompt_groups = HashMap::new();
    let mut artist_string_groups = HashMap::new();
    let mut image_folder_groups: Vec<ImageFolderGroup> = Vec::new();
    let mut image_folder_writer = ImageFolderWriter::new(&output_path);
    let mut failure_folder_writer = FailureFolderWriter::new(&output_path);

    if dedupe_enabled(options) {
        if total_png > 0 {
            progress.emit_progress(
                "extract:file_progress",
                ProgressPayload {
                    total_png: Some(total_png),
                    processed: Some(0),
                    failed: Some(0),
                    skipped_duplicates: Some(0),
                    cache_hits: Some(0),
                    processed_new: Some(0),
                    current_file: None,
                    message: Some(format!("正在使用 {worker_count} 个线程读取 PNG 元数据...")),
                },
            );
        }

        let loaded_images = load_image_states_parallel(
            &images,
            cache.as_ref(),
            prepared_input.cache_match_mode,
            worker_count,
        );

        for (index, loaded_image) in loaded_images.into_iter().enumerate() {
            let LoadedImage {
                source,
                mut image_state,
                cache_hit,
                mut cache_record_dirty,
            } = loaded_image;

            if cache_hit {
                cache_hits += 1;
            } else {
                processed_new += 1;
            }

            let mut file_failed = image_state.metadata_failed;

            if let Some(message) = image_state.metadata_warning.clone() {
                let warning = FileWarning::new(source.display_path.clone(), message);
                progress.emit_warning(&warning);
                warnings.push(warning);
            }

            if let Some(duplicate) = duplicate_match(
                &image_state.metadata.positive_prompt,
                &image_state.metadata.artist_tags,
                options,
                &positive_prompt_groups,
                &artist_string_groups,
            ) {
                skipped_duplicates += 1;

                let group = &mut image_folder_groups[duplicate.group_index];
                group.sort_time = earliest_sort_time(group.sort_time, source.sort_time);
                group.duplicate_sources.push(source.clone());

                if file_failed {
                    failure_folder_writer.remember_failed_source(&source);
                    failed += 1;
                }

                if cache_record_dirty {
                    if let Some(cache_store) = cache.as_mut() {
                        cache_store.save_record(cache_record_from_state(&source, &image_state))?;
                    }
                }

                progress.emit_progress(
                    "extract:file_progress",
                    ProgressPayload {
                        total_png: Some(total_png),
                        processed: Some(index + 1),
                        failed: Some(failed),
                        skipped_duplicates: Some(skipped_duplicates),
                        cache_hits: Some(cache_hits),
                        processed_new: Some(processed_new),
                        current_file: Some(source.display_path.clone()),
                        message: Some(format!(
                            "正在处理 {} / {}，缓存复用 {} 张，新处理 {} 张，已去重跳过 {} 张（{}）",
                            index + 1,
                            total_png,
                            cache_hits,
                            processed_new,
                            skipped_duplicates,
                            duplicate.reason
                        )),
                    },
                );
                continue;
            }

            let thumbnail_result = thumbnail_for_row(
                &source,
                &mut image_state,
                cache.as_ref(),
                &temp_dir.path,
                index,
            );
            if thumbnail_result.1 {
                cache_record_dirty = true;
            }

            match thumbnail_result.0 {
                Ok(thumbnail_path) if !file_failed => {
                    let row_index = rows.len();
                    remember_image_folder_group(
                        &source,
                        row_index,
                        &image_state.metadata.positive_prompt,
                        &image_state.metadata.artist_tags,
                        options,
                        &mut image_folder_groups,
                        &mut positive_prompt_groups,
                        &mut artist_string_groups,
                    );
                    rows.push(WorkbookRow {
                        thumbnail_path,
                        source_path: source_path_for_xlsx(
                            input_path,
                            prepared_input.cache_match_mode,
                            &source,
                        ),
                        sort_time: source.sort_time,
                        sort_time_text: format_time_for_xlsx(source.sort_time),
                        positive_prompt: image_state.metadata.positive_prompt.clone(),
                        negative_prompt: image_state.metadata.negative_prompt.clone(),
                        artist_tags: image_state.metadata.artist_tags.clone(),
                        image_folder: String::new(),
                    });
                }
                Ok(_) => {}
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

            if cache_record_dirty {
                if let Some(cache_store) = cache.as_mut() {
                    cache_store.save_record(cache_record_from_state(&source, &image_state))?;
                }
            }

            if file_failed {
                failure_folder_writer.remember_failed_source(&source);
                failed += 1;
            }

            progress.emit_progress(
                "extract:file_progress",
                ProgressPayload {
                    total_png: Some(total_png),
                    processed: Some(index + 1),
                    failed: Some(failed),
                    skipped_duplicates: Some(skipped_duplicates),
                    cache_hits: Some(cache_hits),
                    processed_new: Some(processed_new),
                    current_file: Some(source.display_path.clone()),
                    message: Some(format!(
                        "正在处理 {} / {}，缓存复用 {} 张，新处理 {} 张",
                        index + 1,
                        total_png,
                        cache_hits,
                        processed_new
                    )),
                },
            );
        }
    } else {
        if total_png > 0 {
            progress.emit_progress(
                "extract:file_progress",
                ProgressPayload {
                    total_png: Some(total_png),
                    processed: Some(0),
                    failed: Some(0),
                    skipped_duplicates: Some(0),
                    cache_hits: Some(0),
                    processed_new: Some(0),
                    current_file: None,
                    message: Some(format!(
                        "正在使用 {worker_count} 个线程读取元数据并生成缩略图..."
                    )),
                },
            );
        }

        let processed_images = process_images_without_dedupe_parallel(
            &images,
            cache.as_ref(),
            prepared_input.cache_match_mode,
            &temp_dir.path,
            worker_count,
        );

        for (index, processed_image) in processed_images.into_iter().enumerate() {
            let ProcessedImage {
                source,
                image_state,
                cache_hit,
                cache_record_dirty,
                thumbnail_result,
            } = processed_image;

            if cache_hit {
                cache_hits += 1;
            } else {
                processed_new += 1;
            }

            let mut file_failed = image_state.metadata_failed;

            if let Some(message) = image_state.metadata_warning.clone() {
                let warning = FileWarning::new(source.display_path.clone(), message);
                progress.emit_warning(&warning);
                warnings.push(warning);
            }

            match thumbnail_result {
                Ok(thumbnail_path) if !file_failed => {
                    let row_index = rows.len();
                    remember_image_folder_group(
                        &source,
                        row_index,
                        &image_state.metadata.positive_prompt,
                        &image_state.metadata.artist_tags,
                        options,
                        &mut image_folder_groups,
                        &mut positive_prompt_groups,
                        &mut artist_string_groups,
                    );
                    rows.push(WorkbookRow {
                        thumbnail_path,
                        source_path: source_path_for_xlsx(
                            input_path,
                            prepared_input.cache_match_mode,
                            &source,
                        ),
                        sort_time: source.sort_time,
                        sort_time_text: format_time_for_xlsx(source.sort_time),
                        positive_prompt: image_state.metadata.positive_prompt.clone(),
                        negative_prompt: image_state.metadata.negative_prompt.clone(),
                        artist_tags: image_state.metadata.artist_tags.clone(),
                        image_folder: String::new(),
                    });
                }
                Ok(_) => {}
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

            if cache_record_dirty {
                if let Some(cache_store) = cache.as_mut() {
                    cache_store.save_record(cache_record_from_state(&source, &image_state))?;
                }
            }

            if file_failed {
                failure_folder_writer.remember_failed_source(&source);
                failed += 1;
            }

            progress.emit_progress(
                "extract:file_progress",
                ProgressPayload {
                    total_png: Some(total_png),
                    processed: Some(index + 1),
                    failed: Some(failed),
                    skipped_duplicates: Some(skipped_duplicates),
                    cache_hits: Some(cache_hits),
                    processed_new: Some(processed_new),
                    current_file: Some(source.display_path.clone()),
                    message: Some(format!(
                        "正在处理 {} / {}，缓存复用 {} 张，新处理 {} 张",
                        index + 1,
                        total_png,
                        cache_hits,
                        processed_new
                    )),
                },
            );
        }
    }

    image_folder_writer
        .write_image_folders(&mut image_folder_groups, &mut rows, options.sort_by_time)
        .context("无法写入图片文件夹")?;
    failure_folder_writer
        .write_failed_sources(options.sort_by_time)
        .context("无法写入失败图片文件夹")?;

    if options.sort_by_time {
        sort_rows_by_time(&mut rows);
    }

    write_xlsx(&rows, &output_path, options.sort_by_time).context("无法生成 Excel 工作簿")?;

    progress.emit_progress(
        "extract:complete",
        ProgressPayload {
            total_png: Some(total_png),
            processed: Some(total_png),
            failed: Some(failed),
            skipped_duplicates: Some(skipped_duplicates),
            cache_hits: Some(cache_hits),
            processed_new: Some(processed_new),
            current_file: None,
            message: Some("处理完成。".to_string()),
        },
    );

    Ok(RunSummary {
        total_png,
        processed: total_png,
        failed,
        skipped_duplicates,
        cache_hits,
        processed_new,
        output_path: output_path.display().to_string(),
        warnings,
    })
}

struct ImageProcessingState {
    metadata: NovelAiMetadata,
    metadata_warning: Option<String>,
    metadata_failed: bool,
    thumbnail_file_name: Option<String>,
    thumbnail_error: Option<String>,
}

struct LoadedImage {
    source: SourceImage,
    image_state: ImageProcessingState,
    cache_hit: bool,
    cache_record_dirty: bool,
}

struct ProcessedImage {
    source: SourceImage,
    image_state: ImageProcessingState,
    cache_hit: bool,
    cache_record_dirty: bool,
    thumbnail_result: Result<PathBuf>,
}

fn dedupe_enabled(options: ExtractionOptions) -> bool {
    options.dedupe_positive_prompt || options.dedupe_artist_tags
}

fn image_worker_count(image_count: usize) -> usize {
    if image_count == 0 {
        return 0;
    }

    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(MAX_IMAGE_WORKER_THREADS)
        .min(image_count)
        .max(1)
}

fn worker_count_description(worker_count: usize) -> String {
    if worker_count == 0 {
        "无需启动处理线程".to_string()
    } else {
        format!("使用 {worker_count} 个处理线程")
    }
}

fn load_image_states_parallel(
    images: &[SourceImage],
    cache: Option<&CacheStore>,
    cache_match_mode: CacheMatchMode,
    worker_count: usize,
) -> Vec<LoadedImage> {
    parallel_map_sources(images, worker_count, |_, source| {
        load_image_state(source, cache, cache_match_mode)
    })
}

fn process_images_without_dedupe_parallel(
    images: &[SourceImage],
    cache: Option<&CacheStore>,
    cache_match_mode: CacheMatchMode,
    temp_dir: &Path,
    worker_count: usize,
) -> Vec<ProcessedImage> {
    parallel_map_sources(images, worker_count, |index, source| {
        let LoadedImage {
            source,
            mut image_state,
            cache_hit,
            mut cache_record_dirty,
        } = load_image_state(source, cache, cache_match_mode);

        let thumbnail_result = thumbnail_for_row(&source, &mut image_state, cache, temp_dir, index);
        if thumbnail_result.1 {
            cache_record_dirty = true;
        }

        ProcessedImage {
            source,
            image_state,
            cache_hit,
            cache_record_dirty,
            thumbnail_result: thumbnail_result.0,
        }
    })
}

fn parallel_map_sources<T, F>(images: &[SourceImage], worker_count: usize, worker: F) -> Vec<T>
where
    T: Send,
    F: Fn(usize, SourceImage) -> T + Sync,
{
    if images.is_empty() {
        return Vec::new();
    }

    if worker_count <= 1 {
        return images
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, source)| worker(index, source))
            .collect();
    }

    let jobs = Arc::new(Mutex::new(
        images.iter().cloned().enumerate().collect::<VecDeque<_>>(),
    ));
    let (sender, receiver) = mpsc::channel();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let jobs = Arc::clone(&jobs);
            let sender = sender.clone();
            let worker = &worker;

            scope.spawn(move || loop {
                let job = jobs
                    .lock()
                    .expect("image worker queue should not be poisoned")
                    .pop_front();
                let Some((index, source)) = job else {
                    break;
                };

                if sender.send((index, worker(index, source))).is_err() {
                    break;
                }
            });
        }
        drop(sender);

        let mut results = (0..images.len()).map(|_| None).collect::<Vec<_>>();
        for (index, result) in receiver {
            results[index] = Some(result);
        }

        results
            .into_iter()
            .map(|result| result.expect("image worker should return one result per image"))
            .collect()
    })
}

fn load_image_state(
    source: SourceImage,
    cache: Option<&CacheStore>,
    cache_match_mode: CacheMatchMode,
) -> LoadedImage {
    let mut image_state = cached_image_state(&source, cache, cache_match_mode);
    let mut cache_record_dirty = false;
    let cache_hit = image_state.is_some();

    if image_state.is_none() {
        image_state = Some(read_image_metadata(&source));
        cache_record_dirty = cache.is_some();
    }

    LoadedImage {
        source,
        image_state: image_state.expect("image state should be loaded or cached"),
        cache_hit,
        cache_record_dirty,
    }
}

fn cached_image_state(
    source: &SourceImage,
    cache: Option<&CacheStore>,
    cache_match_mode: CacheMatchMode,
) -> Option<ImageProcessingState> {
    let record = cache?.get(&source.display_path)?;
    if !cache_record_matches_source(record, source, cache_match_mode) {
        return None;
    }

    Some(ImageProcessingState {
        metadata: NovelAiMetadata {
            positive_prompt: record.positive_prompt.clone(),
            negative_prompt: record.negative_prompt.clone(),
            artist_tags: record.artist_tags.clone(),
        },
        metadata_warning: record.metadata_warning.clone(),
        metadata_failed: record.metadata_failed,
        thumbnail_file_name: record.thumbnail_file_name.clone(),
        thumbnail_error: record.thumbnail_error.clone(),
    })
}

fn cache_record_matches_source(
    record: &CachedImageRecord,
    source: &SourceImage,
    cache_match_mode: CacheMatchMode,
) -> bool {
    if record.source_size != source.fingerprint.size {
        return false;
    }

    match cache_match_mode {
        CacheMatchMode::FileSystem => {
            record.source_modified_nanos == source.fingerprint.modified_nanos
        }
        CacheMatchMode::Archive => true,
    }
}

fn read_image_metadata(source: &SourceImage) -> ImageProcessingState {
    let mut metadata_warning = None;
    let mut metadata_failed = false;

    let metadata = match read_png_text_chunks(&source.absolute_path) {
        Ok(text_chunks) => {
            if text_chunks.is_empty() {
                metadata_warning = Some("未找到 PNG 文本元数据。".to_string());
                metadata_failed = true;
            }
            parse_novelai_metadata(&text_chunks)
        }
        Err(error) => {
            metadata_warning = Some(format!("无法读取 PNG 文本元数据：{error}"));
            metadata_failed = true;
            parse_novelai_metadata(&Default::default())
        }
    };

    ImageProcessingState {
        metadata,
        metadata_warning,
        metadata_failed,
        thumbnail_file_name: None,
        thumbnail_error: None,
    }
}

fn thumbnail_for_row(
    source: &SourceImage,
    image_state: &mut ImageProcessingState,
    cache: Option<&CacheStore>,
    temp_dir: &Path,
    index: usize,
) -> (Result<PathBuf>, bool) {
    let Some(cache_store) = cache else {
        return (
            create_thumbnail(&source.absolute_path, temp_dir, index),
            false,
        );
    };

    if let Some(file_name) = image_state.thumbnail_file_name.clone() {
        let thumbnail_path = cache_store.thumbnail_path_for_file_name(&file_name);
        if thumbnail_path.exists() {
            return (Ok(thumbnail_path), false);
        }
    } else if let Some(error) = image_state.thumbnail_error.clone() {
        return (Err(anyhow::anyhow!(error)), false);
    }

    let thumbnail_file_name = cache_store.thumbnail_file_name(&source.display_path);
    let thumbnail_path = cache_store.thumbnail_path_for_display_path(&source.display_path);
    let thumbnail_result = create_thumbnail_at(&source.absolute_path, &thumbnail_path);

    match thumbnail_result {
        Ok(()) => {
            image_state.thumbnail_file_name = Some(thumbnail_file_name);
            image_state.thumbnail_error = None;
            (Ok(thumbnail_path), true)
        }
        Err(error) => {
            image_state.thumbnail_file_name = None;
            image_state.thumbnail_error = Some(error.to_string());
            (Err(error), true)
        }
    }
}

fn cache_record_from_state(
    source: &SourceImage,
    image_state: &ImageProcessingState,
) -> CachedImageRecord {
    CachedImageRecord {
        display_path: source.display_path.clone(),
        source_size: source.fingerprint.size,
        source_modified_nanos: source.fingerprint.modified_nanos,
        positive_prompt: image_state.metadata.positive_prompt.clone(),
        negative_prompt: image_state.metadata.negative_prompt.clone(),
        artist_tags: image_state.metadata.artist_tags.clone(),
        metadata_warning: image_state.metadata_warning.clone(),
        metadata_failed: image_state.metadata_failed,
        thumbnail_file_name: image_state.thumbnail_file_name.clone(),
        thumbnail_error: image_state.thumbnail_error.clone(),
    }
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

fn remember_image_folder_group(
    source: &SourceImage,
    row_index: usize,
    positive_prompt: &str,
    artist_tags: &[String],
    options: ExtractionOptions,
    image_folder_groups: &mut Vec<ImageFolderGroup>,
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

    let group_index = image_folder_groups.len();
    image_folder_groups.push(ImageFolderGroup {
        row_index,
        representative_path: source.absolute_path.clone(),
        representative_display_path: source.display_path.clone(),
        duplicate_sources: Vec::new(),
        sort_time: source.sort_time,
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

fn output_folder_name(
    base_name: &str,
    sort_time: Option<SystemTime>,
    sort_by_time: bool,
) -> String {
    if sort_by_time {
        format!("{}_{}", format_time_for_folder_prefix(sort_time), base_name)
    } else {
        base_name.to_string()
    }
}

fn earliest_sort_time(
    current: Option<SystemTime>,
    candidate: Option<SystemTime>,
) -> Option<SystemTime> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(if candidate < current {
            candidate
        } else {
            current
        }),
        (None, Some(candidate)) => Some(candidate),
        (Some(current), None) => Some(current),
        (None, None) => None,
    }
}

fn sort_rows_by_time(rows: &mut [WorkbookRow]) {
    rows.sort_by(|left, right| {
        compare_optional_time(left.sort_time, right.sort_time)
            .then_with(|| left.source_path.cmp(&right.source_path))
    });
}

fn source_path_for_xlsx(
    input_path: &Path,
    cache_match_mode: CacheMatchMode,
    source: &SourceImage,
) -> String {
    match cache_match_mode {
        CacheMatchMode::FileSystem => source.absolute_path.display().to_string(),
        CacheMatchMode::Archive => format!("{} > {}", input_path.display(), source.display_path),
    }
}

fn compare_optional_time(left: Option<SystemTime>, right: Option<SystemTime>) -> CmpOrdering {
    match (left, right) {
        (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(CmpOrdering::Equal),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    }
}

fn format_time_for_xlsx(sort_time: Option<SystemTime>) -> String {
    sort_time
        .map(format_local_time_for_xlsx)
        .unwrap_or_default()
}

fn format_time_for_folder_prefix(sort_time: Option<SystemTime>) -> String {
    sort_time
        .map(format_local_time_for_folder_prefix)
        .unwrap_or_else(|| MISSING_TIME_FOLDER_PREFIX.to_string())
}

fn format_local_time_for_xlsx(sort_time: SystemTime) -> String {
    let datetime: DateTime<Local> = sort_time.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn format_local_time_for_folder_prefix(sort_time: SystemTime) -> String {
    let datetime: DateTime<Local> = sort_time.into();
    datetime.format("%Y-%m-%d_%H%M%S").to_string()
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
            let display_path = input_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| input_path.display().to_string());
            return Ok(vec![source_image(input_path, display_path)?]);
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

        images.push(source_image(entry.path(), display_path)?);
    }

    images.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    Ok(images)
}

fn source_image(path: &Path, display_path: String) -> Result<SourceImage> {
    Ok(SourceImage {
        absolute_path: path.to_path_buf(),
        display_path,
        fingerprint: FileFingerprint::from_path(path)?,
        sort_time: file_creation_time(path),
    })
}

fn file_creation_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.created().ok())
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

fn prepare_input(input_path: &Path, temp_dir: &Path) -> Result<PreparedInput> {
    let Some(extension) = archive_extension(input_path) else {
        return Ok(PreparedInput {
            path: input_path.to_path_buf(),
            cache_match_mode: CacheMatchMode::FileSystem,
            archive_fingerprint: None,
        });
    };

    let archive_fingerprint = FileFingerprint::from_path(input_path)?;
    let extract_dir = temp_dir.join("archive");
    fs::create_dir_all(&extract_dir)
        .with_context(|| format!("无法创建解压目录：{}", extract_dir.display()))?;

    match extension.as_str() {
        "zip" => extract_zip_archive(input_path, &extract_dir)?,
        "7z" => extract_7z_archive(input_path, &extract_dir)?,
        "rar" => extract_rar_archive(input_path, &extract_dir)?,
        _ => unreachable!(),
    }

    Ok(PreparedInput {
        path: extract_dir,
        cache_match_mode: CacheMatchMode::Archive,
        archive_fingerprint: Some(archive_fingerprint),
    })
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

    let thumbnail_path = thumbnails_dir.join(format!("{index:06}.png"));
    create_thumbnail_at(source_path, &thumbnail_path)?;

    Ok(thumbnail_path)
}

fn create_thumbnail_at(source_path: &Path, thumbnail_path: &Path) -> Result<()> {
    if let Some(parent) = thumbnail_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建缩略图目录：{}", parent.display()))?;
    }

    let image = image::open(source_path)
        .with_context(|| format!("无法读取图片：{}", source_path.display()))?;
    let thumbnail = image.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE);
    thumbnail
        .save_with_format(thumbnail_path, image::ImageFormat::Png)
        .with_context(|| format!("无法保存缩略图：{}", thumbnail_path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::xlsx::WorkbookRow;
    use super::{
        output_folder_name, run_extraction, run_extraction_with_options, sort_rows_by_time,
        ExtractionOptions, NoopProgressSink, RunSummary,
    };
    use crc32fast::Hasher;
    use image::{Rgb, RgbImage};
    use std::fs;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
        let png_path_text = png_path.display().to_string();
        assert_xlsx_contains(
            &actual_output,
            &[
                "图片文件夹",
                "图片路径",
                "image1/",
                &png_path_text,
                "best quality, artist:demo",
                "bad hands",
            ],
        );
        assert_xlsx_cell_text(&actual_output, "E1", "图片文件夹");
        assert_xlsx_cell_text(&actual_output, "E2", "image1/");
        assert_xlsx_cell_text(&actual_output, "F1", "图片路径");
        assert_xlsx_cell_text(&actual_output, "F2", &png_path_text);
        assert!(!root.join("image1").exists());
        assert_duplicate_folder_contains(
            &actual_output.parent().unwrap().join("image1"),
            &["sample.png"],
        );
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
    fn incremental_reuses_unchanged_folder_records() {
        let root = test_root("incremental_reuses_unchanged_folder_records");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:cached");

        let output = root.join("metadata.xlsx");
        let options = ExtractionOptions {
            incremental: true,
            ..ExtractionOptions::default()
        };
        let first_summary =
            run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();
        assert_eq!(first_summary.total_png, 1);
        assert_eq!(first_summary.cache_hits, 0);
        assert_eq!(first_summary.processed_new, 1);
        assert!(root.join(".novelai_metadata_cache").is_dir());

        let second_summary =
            run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();
        assert_eq!(second_summary.total_png, 1);
        assert_eq!(second_summary.cache_hits, 1);
        assert_eq!(second_summary.processed_new, 0);
        let actual_output = actual_output_path(&second_summary);
        assert_eq!(actual_output, root.join("metadata_1").join("metadata.xlsx"));
        assert_xlsx_contains(&actual_output, &["artist:cached"]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_processes_only_new_folder_files() {
        let root = test_root("incremental_processes_only_new_folder_files");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "artist:first");

        let output = root.join("metadata.xlsx");
        let options = ExtractionOptions {
            incremental: true,
            ..ExtractionOptions::default()
        };
        run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();

        let second_png = input.join("second.png");
        create_test_png(&second_png);
        insert_text_chunk(&second_png, "Description", "artist:second");

        let second_summary =
            run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();
        assert_eq!(second_summary.total_png, 2);
        assert_eq!(second_summary.cache_hits, 1);
        assert_eq!(second_summary.processed_new, 1);
        assert_xlsx_contains(
            &actual_output_path(&second_summary),
            &["artist:first", "artist:second"],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_reuses_metadata_when_dedupe_option_changes() {
        let root = test_root("incremental_reuses_metadata_when_dedupe_option_changes");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "same prompt, artist:cache");

        let second_png = input.join("second.png");
        create_colored_test_png(&second_png, [71, 128, 48]);
        insert_text_chunk(&second_png, "Description", "same prompt, artist:cache");

        let output = root.join("metadata.xlsx");
        let dedupe_options = ExtractionOptions {
            dedupe_positive_prompt: true,
            incremental: true,
            ..ExtractionOptions::default()
        };
        let first_summary =
            run_extraction_with_options(&input, &output, dedupe_options, &NoopProgressSink)
                .unwrap();
        assert_eq!(first_summary.skipped_duplicates, 1);
        assert_eq!(first_summary.cache_hits, 0);
        assert_eq!(first_summary.processed_new, 2);
        assert_xlsx_media_count(&actual_output_path(&first_summary), 1);

        let no_dedupe_options = ExtractionOptions {
            incremental: true,
            ..ExtractionOptions::default()
        };
        let second_summary =
            run_extraction_with_options(&input, &output, no_dedupe_options, &NoopProgressSink)
                .unwrap();
        assert_eq!(second_summary.skipped_duplicates, 0);
        assert_eq!(second_summary.cache_hits, 2);
        assert_eq!(second_summary.processed_new, 0);
        assert_xlsx_media_count(&actual_output_path(&second_summary), 2);

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
    fn sorts_rows_by_time_with_missing_values_last() {
        let early = UNIX_EPOCH + Duration::from_secs(10);
        let late = UNIX_EPOCH + Duration::from_secs(20);
        let mut rows = vec![
            workbook_row("missing.png", None),
            workbook_row("late.png", Some(late)),
            workbook_row("early.png", Some(early)),
        ];

        sort_rows_by_time(&mut rows);

        let ordered_sources = rows
            .iter()
            .map(|row| row.source_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered_sources,
            vec!["early.png", "late.png", "missing.png"]
        );
    }

    #[test]
    fn sorting_option_adds_time_column_and_prefixes_duplicate_folder() {
        let root = test_root("sorting_option_adds_time_column_and_prefixes_duplicate_folder");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "same prompt, artist:sorted");

        let second_png = input.join("second.png");
        create_test_png(&second_png);
        insert_text_chunk(&second_png, "Description", "same prompt, artist:sorted");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                dedupe_positive_prompt: true,
                dedupe_artist_tags: false,
                sort_by_time: true,
                incremental: false,
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 2);
        assert_eq!(summary.skipped_duplicates, 1);
        let actual_output = actual_output_path(&summary);
        let actual_output_dir = actual_output.parent().unwrap();
        let duplicate_folder_name = find_output_subdir(actual_output_dir, "image1");
        assert_sorted_folder_name(&duplicate_folder_name, "image1");
        assert_duplicate_folder_contains(
            &actual_output_dir.join(&duplicate_folder_name),
            &["first.png", "second.png"],
        );

        let duplicate_folder_cell = format!("{duplicate_folder_name}/");
        assert_xlsx_contains(
            &actual_output,
            &[
                "时间",
                duplicate_folder_cell.as_str(),
                "same prompt, artist:sorted",
            ],
        );
        assert_xlsx_cell_text(&actual_output, "G1", "图片路径");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_time_folder_prefix_sorts_last() {
        assert_eq!(
            output_folder_name("image1", None, true),
            "9999-12-31_235959_image1"
        );
        assert_eq!(output_folder_name("image1", None, false), "image1");
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
                sort_by_time: false,
                incremental: false,
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
    fn dedupe_outputs_unique_rows_to_their_own_folders() {
        let root = test_root("dedupe_outputs_unique_rows_to_their_own_folders");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "same prompt, artist:first");

        let second_png = input.join("second.png");
        create_test_png(&second_png);
        insert_text_chunk(&second_png, "Description", "same prompt, artist:first");

        let third_png = input.join("third.png");
        create_colored_test_png(&third_png, [72, 143, 91]);
        insert_text_chunk(&third_png, "Description", "unique prompt, artist:solo");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                dedupe_positive_prompt: true,
                dedupe_artist_tags: false,
                sort_by_time: false,
                incremental: false,
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 3);
        assert_eq!(summary.processed, 3);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped_duplicates, 1);
        let actual_output = actual_output_path(&summary);
        let actual_output_dir = actual_output.parent().unwrap();
        assert_xlsx_media_count(&actual_output, 2);
        assert_xlsx_contains(
            &actual_output,
            &[
                "same prompt, artist:first",
                "unique prompt, artist:solo",
                "image1/",
                "image2/",
            ],
        );
        assert_duplicate_folder_contains(
            &actual_output_dir.join("image1"),
            &["first.png", "second.png"],
        );
        assert_duplicate_folder_contains(&actual_output_dir.join("image2"), &["third.png"]);

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
                sort_by_time: false,
                incremental: false,
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
                sort_by_time: false,
                incremental: false,
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
        assert_xlsx_media_count(&actual_output, 0);
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
        let archive_path_text = format!(r"{} &gt; nested\sample.png", archive_path.display());
        assert_xlsx_contains(
            &actual_output,
            &["图片路径", &archive_path_text, "artist:zip"],
        );

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
        create_colored_test_png(path, [40, 94, 172]);
    }

    fn create_colored_test_png(path: &Path, color: [u8; 3]) {
        let image = RgbImage::from_pixel(4, 4, Rgb(color));
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

    fn workbook_row(source_path: &str, sort_time: Option<SystemTime>) -> WorkbookRow {
        WorkbookRow {
            thumbnail_path: PathBuf::from(format!("{source_path}.thumb.png")),
            source_path: source_path.to_string(),
            sort_time,
            sort_time_text: String::new(),
            positive_prompt: String::new(),
            negative_prompt: String::new(),
            artist_tags: Vec::new(),
            image_folder: String::new(),
        }
    }

    fn find_output_subdir(output_dir: &Path, base_name: &str) -> String {
        fs::read_dir(output_dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .find_map(|entry| {
                let file_type = entry.file_type().unwrap();
                let name = entry.file_name().to_string_lossy().to_string();
                (file_type.is_dir() && name.ends_with(&format!("_{base_name}"))).then_some(name)
            })
            .unwrap_or_else(|| {
                panic!(
                    "{} should contain a sorted {base_name} folder",
                    output_dir.display()
                )
            })
    }

    fn assert_sorted_folder_name(folder_name: &str, base_name: &str) {
        let suffix = format!("_{base_name}");
        let prefix = folder_name
            .strip_suffix(&suffix)
            .unwrap_or_else(|| panic!("{folder_name} should end with {suffix}"));

        assert_eq!(prefix.len(), "2026-05-26_120000".len());
        assert_eq!(prefix.as_bytes()[4], b'-');
        assert_eq!(prefix.as_bytes()[7], b'-');
        assert_eq!(prefix.as_bytes()[10], b'_');
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

    fn assert_xlsx_cell_text(output: &Path, cell: &str, expected_text: &str) {
        let file = File::open(output).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();

        let mut shared_strings = String::new();
        archive
            .by_name("xl/sharedStrings.xml")
            .unwrap()
            .read_to_string(&mut shared_strings)
            .unwrap();

        let expected_xml = escape_xml_text(expected_text);
        let shared_string_index = shared_strings
            .split("<si>")
            .skip(1)
            .position(|entry| entry.contains(&expected_xml))
            .unwrap_or_else(|| panic!("missing shared string: {expected_text}"));

        let mut sheet = String::new();
        archive
            .by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut sheet)
            .unwrap();

        let cell_marker = format!(r#"<c r="{cell}" "#);
        let cell_start = sheet
            .find(&cell_marker)
            .unwrap_or_else(|| panic!("missing cell: {cell}"));
        let cell_xml = &sheet[cell_start..];
        let cell_end = cell_xml
            .find("</c>")
            .unwrap_or_else(|| panic!("cell has no closing tag: {cell}"));
        let cell_xml = &cell_xml[..cell_end + "</c>".len()];
        assert!(
            cell_xml.contains(&format!("<v>{shared_string_index}</v>")),
            "{cell} did not contain expected text {expected_text}; cell xml: {cell_xml}"
        );
    }

    fn escape_xml_text(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
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
