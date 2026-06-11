use super::cache::{
    clear_cache_root, system_time_to_nanos, CacheClearSummary, CacheStore, CachedImageRecord,
    FileFingerprint, CACHE_DIR_NAME,
};
use super::metadata::{parse_novelai_metadata, NovelAiMetadata};
use super::png_text::{read_png_text_chunks, read_png_text_chunks_from_reader};
use super::xlsx::{write_xlsx, WorkbookRow};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::Serialize;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::fs::File;
use std::io;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unrar_ng::Archive;
use walkdir::WalkDir;
use zip::ZipArchive;

const THUMBNAIL_SIZE: u32 = 160;
const TEMP_ROOT: &str = r"D:\Agent\Agent_temp";
const MISSING_TIME_FOLDER_PREFIX: &str = "9999-12-31_235959";
const MAX_IMAGE_WORKER_THREADS: usize = 32;
const OUTPUT_MARKER_FILE_NAME: &str = ".novelai_metadata_output";
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageOutputMode {
    /// 复制原图到输出包（默认，保持旧行为）。
    #[default]
    Copy,
    /// 创建硬链接，失败时回退为复制（适合输入输出在同一 NTFS 分区的大图库）。
    Hardlink,
    /// 不输出图片文件夹，仅生成 XLSX。
    Skip,
}

impl ImageOutputMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "copy" => Some(Self::Copy),
            "hardlink" => Some(Self::Hardlink),
            "none" => Some(Self::Skip),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractionOptions {
    pub dedupe_positive_prompt: bool,
    pub dedupe_artist_tags: bool,
    pub sort_by_time: bool,
    pub incremental: bool,
    pub image_output_mode: ImageOutputMode,
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
    mode: ImageOutputMode,
    hardlink_fallbacks: usize,
}

struct FailureFolderWriter {
    output_dir: PathBuf,
    failed_sources: Vec<SourceImage>,
    sort_time: Option<SystemTime>,
    copied_count: usize,
    mode: ImageOutputMode,
    hardlink_fallbacks: usize,
}

impl ImageFolderWriter {
    fn new(output_path: &Path, mode: ImageOutputMode) -> Self {
        Self {
            output_dir: output_directory(output_path),
            next_folder_number: 1,
            mode,
            hardlink_fallbacks: 0,
        }
    }

    fn write_image_folders(
        &mut self,
        groups: &mut [ImageFolderGroup],
        rows: &mut [WorkbookRow],
        sort_by_time: bool,
    ) -> Result<()> {
        if self.mode == ImageOutputMode::Skip {
            return Ok(());
        }

        for group in groups.iter_mut() {
            let (folder_name, folder_path) = self.create_folder(group.sort_time, sort_by_time)?;
            transfer_source_to_numbered_folder(
                self.mode,
                &group.representative_path,
                &group.representative_display_path,
                &folder_path,
                &mut group.copied_count,
                "图片",
                &mut self.hardlink_fallbacks,
            )?;

            for source in &group.duplicate_sources {
                transfer_source_to_numbered_folder(
                    self.mode,
                    &source.absolute_path,
                    &source.display_path,
                    &folder_path,
                    &mut group.copied_count,
                    "图片",
                    &mut self.hardlink_fallbacks,
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
    fn new(output_path: &Path, mode: ImageOutputMode) -> Self {
        Self {
            output_dir: output_directory(output_path),
            failed_sources: Vec::new(),
            sort_time: None,
            copied_count: 0,
            mode,
            hardlink_fallbacks: 0,
        }
    }

    fn remember_failed_source(&mut self, source: &SourceImage) {
        self.sort_time = earliest_sort_time(self.sort_time, source.sort_time);
        self.failed_sources.push(source.clone());
    }

    fn write_failed_sources(&mut self, sort_by_time: bool) -> Result<()> {
        if self.mode == ImageOutputMode::Skip || self.failed_sources.is_empty() {
            return Ok(());
        }

        let base_folder_name = if sort_by_time { "Fail" } else { "_Fail" };
        let folder_name = output_folder_name(base_folder_name, self.sort_time, sort_by_time);
        let folder_path = self.output_dir.join(folder_name);
        fs::create_dir(&folder_path)
            .with_context(|| format!("无法创建失败图片文件夹：{}", folder_path.display()))?;

        for source in &self.failed_sources {
            transfer_source_to_numbered_folder(
                self.mode,
                &source.absolute_path,
                &source.display_path,
                &folder_path,
                &mut self.copied_count,
                "失败图片",
                &mut self.hardlink_fallbacks,
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
    let images = collect_png_files(&prepared_input.path, output_path)?;
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
    let mut image_folder_writer = ImageFolderWriter::new(&output_path, options.image_output_mode);
    let mut failure_folder_writer =
        FailureFolderWriter::new(&output_path, options.image_output_mode);

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

        let mut metadata_throttle = ProgressThrottle::new();
        let loaded_images = parallel_map(
            images.clone(),
            worker_count,
            |_, source| load_image_state(source, cache.as_ref(), prepared_input.cache_match_mode),
            |completed| {
                if metadata_throttle.should_emit(completed == total_png) {
                    progress.emit_progress(
                        "extract:file_progress",
                        ProgressPayload {
                            total_png: Some(total_png),
                            processed: Some(completed),
                            failed: None,
                            skipped_duplicates: None,
                            cache_hits: None,
                            processed_new: None,
                            current_file: None,
                            message: Some(format!(
                                "正在读取 PNG 元数据 {completed} / {total_png}..."
                            )),
                        },
                    );
                }
            },
        );

        // 第一阶段：按扫描顺序完成去重分组，只依赖元数据，保证代表图选择和分组编号稳定。
        let mut group_drafts: Vec<GroupDraft> = Vec::new();
        let mut dedupe_throttle = ProgressThrottle::new();

        for (index, loaded_image) in loaded_images.into_iter().enumerate() {
            let LoadedImage {
                source,
                image_state,
                cache_hit,
            } = loaded_image?;

            if cache_hit {
                cache_hits += 1;
            } else {
                processed_new += 1;
            }

            if let Some(message) = image_state.metadata_warning.clone() {
                let warning = FileWarning::new(source.display_path.clone(), message);
                progress.emit_warning(&warning);
                warnings.push(warning);
            }

            let current_file = source.display_path.clone();

            if let Some(duplicate) = duplicate_match(
                &image_state.metadata.positive_prompt,
                &image_state.metadata.artist_tags,
                options,
                &positive_prompt_groups,
                &artist_string_groups,
            ) {
                skipped_duplicates += 1;

                if image_state.metadata_failed {
                    failure_folder_writer.remember_failed_source(&source);
                    failed += 1;
                }

                let draft = &mut group_drafts[duplicate.group_index];
                draft.sort_time = earliest_sort_time(draft.sort_time, source.sort_time);
                draft.duplicate_sources.push(source);
            } else if image_state.metadata_failed {
                failure_folder_writer.remember_failed_source(&source);
                failed += 1;
            } else {
                register_group_keys(
                    &image_state.metadata.positive_prompt,
                    &image_state.metadata.artist_tags,
                    options,
                    group_drafts.len(),
                    &mut positive_prompt_groups,
                    &mut artist_string_groups,
                );
                group_drafts.push(GroupDraft {
                    sort_time: source.sort_time,
                    representative: source,
                    representative_state: image_state,
                    duplicate_sources: Vec::new(),
                });
            }

            if dedupe_throttle.should_emit(index + 1 == total_png) {
                progress.emit_progress(
                    "extract:file_progress",
                    ProgressPayload {
                        total_png: Some(total_png),
                        processed: Some(index + 1),
                        failed: Some(failed),
                        skipped_duplicates: Some(skipped_duplicates),
                        cache_hits: Some(cache_hits),
                        processed_new: Some(processed_new),
                        current_file: Some(current_file),
                        message: Some(format!(
                            "正在去重分析 {} / {}，缓存复用 {} 张，新处理 {} 张，去重跳过 {} 张",
                            index + 1,
                            total_png,
                            cache_hits,
                            processed_new,
                            skipped_duplicates
                        )),
                    },
                );
            }
        }

        // 第二阶段：并行为各组代表图生成缩略图（分组已固定，不再影响输出顺序）。
        let thumbnail_total = group_drafts.len();
        let thumbnail_worker_count = image_worker_count(thumbnail_total);
        if thumbnail_total > 0 {
            progress.emit_progress(
                "extract:file_progress",
                ProgressPayload {
                    total_png: Some(total_png),
                    processed: Some(total_png),
                    failed: None,
                    skipped_duplicates: None,
                    cache_hits: None,
                    processed_new: None,
                    current_file: None,
                    message: Some(format!(
                        "正在使用 {thumbnail_worker_count} 个线程生成 {thumbnail_total} 张缩略图..."
                    )),
                },
            );
        }

        let mut thumbnail_throttle = ProgressThrottle::new();
        let thumbnailed_drafts = parallel_map(
            group_drafts,
            thumbnail_worker_count,
            |index, mut draft: GroupDraft| -> Result<(GroupDraft, Result<PathBuf>)> {
                let (thumbnail_result, cache_record_dirty) = thumbnail_for_row(
                    &draft.representative,
                    &mut draft.representative_state,
                    cache.as_ref(),
                    &temp_dir.path,
                    index,
                    None,
                );
                if cache_record_dirty {
                    if let Some(cache_store) = cache.as_ref() {
                        cache_store.save_record_file(&cache_record_from_state(
                            &draft.representative,
                            &draft.representative_state,
                        ))?;
                    }
                }
                Ok((draft, thumbnail_result))
            },
            |completed| {
                if thumbnail_throttle.should_emit(completed == thumbnail_total) {
                    progress.emit_progress(
                        "extract:file_progress",
                        ProgressPayload {
                            total_png: Some(total_png),
                            processed: Some(total_png),
                            failed: None,
                            skipped_duplicates: None,
                            cache_hits: None,
                            processed_new: None,
                            current_file: None,
                            message: Some(format!(
                                "正在生成缩略图 {completed} / {thumbnail_total}..."
                            )),
                        },
                    );
                }
            },
        );

        // 第三阶段：按组顺序生成行；代表图缩略图失败时依次提升组内重复图为新代表。
        let mut promotion_counter = 0_usize;
        for thumbnailed_draft in thumbnailed_drafts {
            let (mut draft, thumbnail_result) = thumbnailed_draft?;

            let thumbnail_path = match thumbnail_result {
                Ok(thumbnail_path) => Some(thumbnail_path),
                Err(error) => {
                    let warning = FileWarning::new(
                        draft.representative.display_path.clone(),
                        format!("无法创建缩略图：{error}"),
                    );
                    progress.emit_warning(&warning);
                    warnings.push(warning);
                    failure_folder_writer.remember_failed_source(&draft.representative);
                    failed += 1;

                    let mut promoted = None;
                    let mut survivors = Vec::new();
                    let mut candidates =
                        std::mem::take(&mut draft.duplicate_sources).into_iter();
                    for candidate in candidates.by_ref() {
                        let mut candidate_state = image_state_for_source(
                            &candidate,
                            cache.as_ref(),
                            prepared_input.cache_match_mode,
                        );
                        if candidate_state.metadata_failed {
                            // 元数据失败的重复图已进入失败目录，保留在组内即可。
                            survivors.push(candidate);
                            continue;
                        }

                        promotion_counter += 1;
                        let (candidate_result, candidate_dirty) = thumbnail_for_row(
                            &candidate,
                            &mut candidate_state,
                            cache.as_ref(),
                            &temp_dir.path,
                            total_png + promotion_counter,
                            None,
                        );
                        if candidate_dirty {
                            if let Some(cache_store) = cache.as_mut() {
                                cache_store.save_record(cache_record_from_state(
                                    &candidate,
                                    &candidate_state,
                                ))?;
                            }
                        }

                        skipped_duplicates = skipped_duplicates.saturating_sub(1);
                        match candidate_result {
                            Ok(candidate_thumbnail) => {
                                promoted = Some((candidate, candidate_state, candidate_thumbnail));
                                break;
                            }
                            Err(error) => {
                                let warning = FileWarning::new(
                                    candidate.display_path.clone(),
                                    format!("无法创建缩略图：{error}"),
                                );
                                progress.emit_warning(&warning);
                                warnings.push(warning);
                                failure_folder_writer.remember_failed_source(&candidate);
                                failed += 1;
                            }
                        }
                    }
                    survivors.extend(candidates);
                    draft.duplicate_sources = survivors;

                    match promoted {
                        Some((source, image_state, candidate_thumbnail)) => {
                            draft.representative = source;
                            draft.representative_state = image_state;
                            Some(candidate_thumbnail)
                        }
                        None => None,
                    }
                }
            };

            let Some(thumbnail_path) = thumbnail_path else {
                // 组内没有可用代表图：失败图片已记录，整组不输出。
                continue;
            };

            let row_index = rows.len();
            image_folder_groups.push(ImageFolderGroup {
                row_index,
                representative_path: draft.representative.absolute_path.clone(),
                representative_display_path: draft.representative.display_path.clone(),
                duplicate_sources: std::mem::take(&mut draft.duplicate_sources),
                sort_time: draft.sort_time,
                copied_count: 0,
            });
            rows.push(WorkbookRow {
                thumbnail_path,
                source_path: source_path_for_xlsx(
                    input_path,
                    prepared_input.cache_match_mode,
                    &draft.representative,
                ),
                sort_time: draft.representative.sort_time,
                sort_time_text: format_time_for_xlsx(draft.representative.sort_time),
                positive_prompt: draft.representative_state.metadata.positive_prompt.clone(),
                negative_prompt: draft.representative_state.metadata.negative_prompt.clone(),
                artist_tags: draft.representative_state.metadata.artist_tags.clone(),
                image_folder: String::new(),
            });
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

        let mut process_throttle = ProgressThrottle::new();
        let processed_images = parallel_map(
            images.clone(),
            worker_count,
            |index, source| {
                process_image_without_dedupe(
                    index,
                    source,
                    cache.as_ref(),
                    prepared_input.cache_match_mode,
                    &temp_dir.path,
                )
            },
            |completed| {
                if process_throttle.should_emit(completed == total_png) {
                    progress.emit_progress(
                        "extract:file_progress",
                        ProgressPayload {
                            total_png: Some(total_png),
                            processed: Some(completed),
                            failed: None,
                            skipped_duplicates: None,
                            cache_hits: None,
                            processed_new: None,
                            current_file: None,
                            message: Some(format!("正在处理 {completed} / {total_png}...")),
                        },
                    );
                }
            },
        );

        for processed_image in processed_images {
            let ProcessedImage {
                source,
                image_state,
                cache_hit,
                thumbnail_result,
            } = processed_image?;

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
                Some(Ok(thumbnail_path)) => {
                    let row_index = rows.len();
                    remember_image_folder_group(&source, row_index, &mut image_folder_groups);
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
                Some(Err(error)) => {
                    file_failed = true;
                    let warning = FileWarning::new(
                        source.display_path.clone(),
                        format!("无法创建缩略图：{error}"),
                    );
                    progress.emit_warning(&warning);
                    warnings.push(warning);
                }
                None => {}
            }

            if file_failed {
                failure_folder_writer.remember_failed_source(&source);
                failed += 1;
            }
        }

        progress.emit_progress(
            "extract:file_progress",
            ProgressPayload {
                total_png: Some(total_png),
                processed: Some(total_png),
                failed: Some(failed),
                skipped_duplicates: Some(skipped_duplicates),
                cache_hits: Some(cache_hits),
                processed_new: Some(processed_new),
                current_file: None,
                message: Some(format!(
                    "处理完成 {total_png} 张，缓存复用 {cache_hits} 张，新处理 {processed_new} 张"
                )),
            },
        );
    }

    image_folder_writer
        .write_image_folders(&mut image_folder_groups, &mut rows, options.sort_by_time)
        .context("无法写入图片文件夹")?;
    failure_folder_writer
        .write_failed_sources(options.sort_by_time)
        .context("无法写入失败图片文件夹")?;

    let hardlink_fallbacks =
        image_folder_writer.hardlink_fallbacks + failure_folder_writer.hardlink_fallbacks;
    if hardlink_fallbacks > 0 {
        let warning = FileWarning::new(
            "硬链接输出",
            format!(
                "{hardlink_fallbacks} 张图片无法创建硬链接，已回退为复制（输出与图片可能不在同一分区）。"
            ),
        );
        progress.emit_warning(&warning);
        warnings.push(warning);
    }

    if options.sort_by_time {
        sort_rows_by_time(&mut rows);
    }

    write_xlsx(&rows, &output_path, options.sort_by_time).context("无法生成 Excel 工作簿")?;

    // 增量模式下清理已从输入中消失的图片对应的缓存记录和缩略图，避免缓存无限增长。
    let mut pruned_cache_files = 0_usize;
    if let Some(cache_store) = cache.as_mut() {
        let current_display_paths: HashSet<String> = images
            .iter()
            .map(|image| image.display_path.clone())
            .collect();
        pruned_cache_files = cache_store.prune_missing(&current_display_paths)?;
    }

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
            message: Some(if pruned_cache_files > 0 {
                format!("处理完成，已清理 {pruned_cache_files} 个失效缓存文件。")
            } else {
                "处理完成。".to_string()
            }),
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
}

struct ProcessedImage {
    source: SourceImage,
    image_state: ImageProcessingState,
    cache_hit: bool,
    /// `None` 表示元数据失败、未尝试生成缩略图。
    thumbnail_result: Option<Result<PathBuf>>,
}

/// 去重分组草稿：分组先于缩略图生成确定，便于缩略图并行化。
struct GroupDraft {
    representative: SourceImage,
    representative_state: ImageProcessingState,
    duplicate_sources: Vec<SourceImage>,
    sort_time: Option<SystemTime>,
}

struct ProgressThrottle {
    last_emit: Option<Instant>,
}

impl ProgressThrottle {
    fn new() -> Self {
        Self { last_emit: None }
    }

    fn should_emit(&mut self, force: bool) -> bool {
        let now = Instant::now();
        if force
            || self
                .last_emit
                .map_or(true, |last| now.duration_since(last) >= PROGRESS_MIN_INTERVAL)
        {
            self.last_emit = Some(now);
            true
        } else {
            false
        }
    }
}

pub fn clear_metadata_cache_for_output(output_path: &Path) -> Result<CacheClearSummary> {
    clear_cache_root(&output_directory(output_path))
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

#[cfg(test)]
fn load_image_states_parallel(
    images: &[SourceImage],
    cache: Option<&CacheStore>,
    cache_match_mode: CacheMatchMode,
    worker_count: usize,
) -> Vec<Result<LoadedImage>> {
    parallel_map(
        images.to_vec(),
        worker_count,
        |_, source| load_image_state(source, cache, cache_match_mode),
        |_| {},
    )
}

fn process_image_without_dedupe(
    index: usize,
    source: SourceImage,
    cache: Option<&CacheStore>,
    cache_match_mode: CacheMatchMode,
    temp_dir: &Path,
) -> Result<ProcessedImage> {
    let cached_state = cached_image_state(&source, cache, cache_match_mode);
    let cache_hit = cached_state.is_some();
    let (mut image_state, preloaded_bytes) = match cached_state {
        Some(image_state) => (image_state, None),
        None => {
            // 新图片只读一次磁盘：同一份字节既用于解析元数据，也用于生成缩略图。
            let (image_state, bytes) = read_image_metadata_and_bytes(&source);
            if let Some(cache_store) = cache {
                cache_store.save_record_file(&cache_record_from_state(&source, &image_state))?;
            }
            (image_state, bytes)
        }
    };

    let thumbnail_result = if image_state.metadata_failed {
        None
    } else {
        let (thumbnail_result, cache_record_dirty) = thumbnail_for_row(
            &source,
            &mut image_state,
            cache,
            temp_dir,
            index,
            preloaded_bytes.as_deref(),
        );
        if cache_record_dirty {
            if let Some(cache_store) = cache {
                cache_store.save_record_file(&cache_record_from_state(&source, &image_state))?;
            }
        }
        Some(thumbnail_result)
    };

    Ok(ProcessedImage {
        source,
        image_state,
        cache_hit,
        thumbnail_result,
    })
}

fn parallel_map<I, T, F, C>(
    items: Vec<I>,
    worker_count: usize,
    worker: F,
    mut on_complete: C,
) -> Vec<T>
where
    I: Send,
    T: Send,
    F: Fn(usize, I) -> T + Sync,
    C: FnMut(usize),
{
    if items.is_empty() {
        return Vec::new();
    }
    let total = items.len();

    if worker_count <= 1 {
        return items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let result = worker(index, item);
                on_complete(index + 1);
                result
            })
            .collect();
    }

    let jobs = Arc::new(Mutex::new(
        items.into_iter().enumerate().collect::<VecDeque<_>>(),
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
                let Some((index, item)) = job else {
                    break;
                };

                if sender.send((index, worker(index, item))).is_err() {
                    break;
                }
            });
        }
        drop(sender);

        let mut results = (0..total).map(|_| None).collect::<Vec<_>>();
        let mut completed = 0_usize;
        for (index, result) in receiver {
            results[index] = Some(result);
            completed += 1;
            on_complete(completed);
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
) -> Result<LoadedImage> {
    if let Some(image_state) = cached_image_state(&source, cache, cache_match_mode) {
        return Ok(LoadedImage {
            source,
            image_state,
            cache_hit: true,
        });
    }

    let image_state = read_image_metadata(&source);
    if let Some(cache_store) = cache {
        cache_store.save_record_file(&cache_record_from_state(&source, &image_state))?;
    }

    Ok(LoadedImage {
        source,
        image_state,
        cache_hit: false,
    })
}

fn image_state_for_source(
    source: &SourceImage,
    cache: Option<&CacheStore>,
    cache_match_mode: CacheMatchMode,
) -> ImageProcessingState {
    cached_image_state(source, cache, cache_match_mode)
        .unwrap_or_else(|| read_image_metadata(source))
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
    image_state_from_chunks(read_png_text_chunks(&source.absolute_path))
}

/// 读取整个文件并解析元数据，原始字节同时返回给调用方复用（生成缩略图时无需再次读盘）。
fn read_image_metadata_and_bytes(source: &SourceImage) -> (ImageProcessingState, Option<Vec<u8>>) {
    match fs::read(&source.absolute_path) {
        Ok(bytes) => {
            let image_state = image_state_from_chunks(read_png_text_chunks_from_reader(
                Cursor::new(bytes.as_slice()),
            ));
            (image_state, Some(bytes))
        }
        Err(error) => (
            image_state_from_chunks(Err(anyhow::anyhow!(
                "无法打开 PNG 文件：{}：{error}",
                source.absolute_path.display()
            ))),
            None,
        ),
    }
}

fn image_state_from_chunks(text_chunks: Result<BTreeMap<String, String>>) -> ImageProcessingState {
    let mut metadata_warning = None;
    let mut metadata_failed = false;

    let metadata = match text_chunks {
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
    preloaded_bytes: Option<&[u8]>,
) -> (Result<PathBuf>, bool) {
    let Some(cache_store) = cache else {
        return (
            create_thumbnail(&source.absolute_path, temp_dir, index, preloaded_bytes),
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
    let thumbnail_result =
        create_thumbnail_at(&source.absolute_path, &thumbnail_path, preloaded_bytes);

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
                });
            }
        }
    }

    if options.dedupe_artist_tags {
        if let Some(key) = artist_tags_key(artist_tags) {
            if let Some(group_index) = artist_string_groups.get(&key) {
                return Some(DuplicateMatch {
                    group_index: *group_index,
                });
            }
        }
    }

    None
}

fn remember_image_folder_group(
    source: &SourceImage,
    row_index: usize,
    image_folder_groups: &mut Vec<ImageFolderGroup>,
) {
    image_folder_groups.push(ImageFolderGroup {
        row_index,
        representative_path: source.absolute_path.clone(),
        representative_display_path: source.display_path.clone(),
        duplicate_sources: Vec::new(),
        sort_time: source.sort_time,
        copied_count: 0,
    });
}

fn register_group_keys(
    positive_prompt: &str,
    artist_tags: &[String],
    options: ExtractionOptions,
    group_index: usize,
    positive_prompt_groups: &mut HashMap<String, usize>,
    artist_string_groups: &mut HashMap<String, usize>,
) {
    if options.dedupe_positive_prompt {
        if let Some(key) = positive_prompt_key(positive_prompt) {
            positive_prompt_groups.insert(key, group_index);
        }
    }

    if options.dedupe_artist_tags {
        if let Some(key) = artist_tags_key(artist_tags) {
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
    write_output_package_marker(&output_dir)?;
    let file_name = output_package_file_name(requested_output_path);

    Ok(output_dir.join(file_name))
}

fn write_output_package_marker(output_dir: &Path) -> Result<()> {
    let marker_path = output_dir.join(OUTPUT_MARKER_FILE_NAME);
    fs::write(&marker_path, b"NovelAI metadata organizer output\n").with_context(|| {
        format!(
            "Failed to write output package marker: {}",
            marker_path.display()
        )
    })?;
    Ok(())
}

fn output_package_file_name(requested_output_path: &Path) -> String {
    requested_output_path
        .file_name()
        .map(|value| sanitize_file_name(&value.to_string_lossy()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "novelai_metadata.xlsx".to_string())
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

fn transfer_source_to_numbered_folder(
    mode: ImageOutputMode,
    source_path: &Path,
    display_path: &str,
    folder_path: &Path,
    copied_count: &mut usize,
    item_label: &str,
    hardlink_fallbacks: &mut usize,
) -> Result<()> {
    *copied_count += 1;

    let source_file_name = source_path
        .file_name()
        .map(|value| sanitize_file_name(&value.to_string_lossy()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "image.png".to_string());
    let target_path = folder_path.join(format!("{:04}_{source_file_name}", *copied_count));

    if mode == ImageOutputMode::Hardlink {
        if fs::hard_link(source_path, &target_path).is_ok() {
            return Ok(());
        }
        // 硬链接失败（常见于跨分区或非 NTFS），回退为复制。
        *hardlink_fallbacks += 1;
    }

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

struct ScanExclusions {
    output_parent: PathBuf,
    output_parent_inside_input: bool,
    output_folder_name: String,
    output_file_name: String,
}

impl ScanExclusions {
    fn new(input_path: &Path, requested_output_path: &Path) -> Self {
        let input_root = canonical_or_original(input_path);
        let output_parent = canonical_or_original(&output_directory(requested_output_path));
        let output_parent_inside_input =
            output_parent == input_root || output_parent.starts_with(&input_root);

        Self {
            output_parent,
            output_parent_inside_input,
            output_folder_name: output_package_folder_name(requested_output_path),
            output_file_name: output_package_file_name(requested_output_path),
        }
    }
}

fn should_skip_scan_entry(entry: &walkdir::DirEntry, exclusions: &ScanExclusions) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }

    let file_name = entry.file_name().to_string_lossy();
    if file_name.eq_ignore_ascii_case(CACHE_DIR_NAME) {
        return true;
    }

    if !exclusions.output_parent_inside_input
        || !is_direct_child_of_output_parent(entry.path(), &exclusions.output_parent)
        || !is_output_package_dir_name(&file_name, &exclusions.output_folder_name)
    {
        return false;
    }

    entry.path().join(OUTPUT_MARKER_FILE_NAME).exists()
        || entry.path().join(&exclusions.output_file_name).is_file()
}

fn is_direct_child_of_output_parent(path: &Path, output_parent: &Path) -> bool {
    path.parent()
        .map(canonical_or_original)
        .is_some_and(|parent| parent == output_parent)
}

fn is_output_package_dir_name(dir_name: &str, base_name: &str) -> bool {
    if dir_name == base_name {
        return true;
    }

    dir_name
        .strip_prefix(base_name)
        .and_then(|suffix| suffix.strip_prefix('_'))
        .is_some_and(|number| !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit()))
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn collect_png_files(input_path: &Path, requested_output_path: &Path) -> Result<Vec<SourceImage>> {
    if input_path.is_file() {
        if is_png(input_path) {
            let display_path = input_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| input_path.display().to_string());
            let metadata = fs::metadata(input_path)
                .with_context(|| format!("无法读取文件信息：{}", input_path.display()))?;
            return Ok(vec![source_image_from_metadata(
                input_path,
                display_path,
                &metadata,
            )]);
        }

        if is_supported_archive(input_path) {
            bail!("压缩包输入未能解压。");
        }

        bail!("输入文件必须是 PNG、.zip、.7z 或 .rar。");
    }

    if !input_path.is_dir() {
        bail!("输入路径必须是文件夹、PNG 文件或受支持的压缩包。");
    }

    let exclusions = ScanExclusions::new(input_path, requested_output_path);
    let mut images = Vec::new();
    for entry in WalkDir::new(input_path)
        .into_iter()
        .filter_entry(|entry| !should_skip_scan_entry(entry, &exclusions))
    {
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

        // Windows 上 walkdir 的 metadata 来自目录遍历结果，单次调用同时取大小、修改和创建时间。
        let metadata = entry
            .metadata()
            .with_context(|| format!("无法读取文件信息：{}", entry.path().display()))?;
        images.push(source_image_from_metadata(
            entry.path(),
            display_path,
            &metadata,
        ));
    }

    images.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    Ok(images)
}

fn source_image_from_metadata(
    path: &Path,
    display_path: String,
    metadata: &fs::Metadata,
) -> SourceImage {
    SourceImage {
        absolute_path: path.to_path_buf(),
        display_path,
        fingerprint: FileFingerprint {
            size: metadata.len(),
            modified_nanos: metadata.modified().ok().and_then(system_time_to_nanos),
        },
        sort_time: metadata.created().ok(),
    }
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

fn create_thumbnail(
    source_path: &Path,
    temp_dir: &Path,
    index: usize,
    preloaded_bytes: Option<&[u8]>,
) -> Result<PathBuf> {
    let thumbnails_dir = temp_dir.join("thumbnails");
    fs::create_dir_all(&thumbnails_dir)
        .with_context(|| format!("无法创建缩略图临时目录：{}", thumbnails_dir.display()))?;

    let thumbnail_path = thumbnails_dir.join(format!("{index:06}.png"));
    create_thumbnail_at(source_path, &thumbnail_path, preloaded_bytes)?;

    Ok(thumbnail_path)
}

fn create_thumbnail_at(
    source_path: &Path,
    thumbnail_path: &Path,
    preloaded_bytes: Option<&[u8]>,
) -> Result<()> {
    if let Some(parent) = thumbnail_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建缩略图目录：{}", parent.display()))?;
    }

    let image = match preloaded_bytes {
        Some(bytes) => image::load_from_memory(bytes)
            .with_context(|| format!("无法读取图片：{}", source_path.display()))?,
        None => image::open(source_path)
            .with_context(|| format!("无法读取图片：{}", source_path.display()))?,
    };
    let thumbnail = image.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE);
    thumbnail
        .save_with_format(thumbnail_path, image::ImageFormat::Png)
        .with_context(|| format!("无法保存缩略图：{}", thumbnail_path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::cache::CacheStore;
    use super::super::xlsx::WorkbookRow;
    use super::{
        clear_metadata_cache_for_output, collect_png_files, image_worker_count,
        load_image_states_parallel, output_folder_name, run_extraction,
        run_extraction_with_options, sort_rows_by_time, CacheMatchMode, ExtractionOptions,
        ImageOutputMode, NoopProgressSink, RunSummary,
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
    fn incremental_ignores_generated_output_inside_input_folder() {
        let root = test_root("incremental_ignores_generated_output_inside_input_folder");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:inside-output");

        let output = input.join("metadata.xlsx");
        let options = ExtractionOptions {
            incremental: true,
            ..ExtractionOptions::default()
        };
        let first_summary =
            run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();
        assert_eq!(first_summary.total_png, 1);
        assert_eq!(first_summary.processed_new, 1);
        assert!(input
            .join("metadata")
            .join(".novelai_metadata_output")
            .exists());

        let second_summary =
            run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();
        assert_eq!(second_summary.total_png, 1);
        assert_eq!(second_summary.cache_hits, 1);
        assert_eq!(second_summary.processed_new, 0);
        assert_eq!(
            actual_output_path(&second_summary),
            input.join("metadata_1").join("metadata.xlsx")
        );
        assert_xlsx_contains(
            &actual_output_path(&second_summary),
            &["artist:inside-output"],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_persists_records_during_parallel_metadata_load() {
        let root = test_root("incremental_persists_records_during_parallel_metadata_load");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "artist:first-cache");

        let second_png = input.join("second.png");
        create_colored_test_png(&second_png, [71, 128, 48]);
        insert_text_chunk(&second_png, "Description", "artist:second-cache");

        let output = root.join("metadata.xlsx");
        let images = collect_png_files(&input, &output).unwrap();
        assert_eq!(images.len(), 2);

        let cache = CacheStore::open(&root, &input, None).unwrap();
        assert_eq!(cache.record_count(), 0);

        let loaded_images = load_image_states_parallel(
            &images,
            Some(&cache),
            CacheMatchMode::FileSystem,
            image_worker_count(images.len()),
        );
        for loaded_image in loaded_images {
            loaded_image.unwrap();
        }

        let reopened_cache = CacheStore::open(&root, &input, None).unwrap();
        assert_eq!(reopened_cache.record_count(), 2);

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
                ..ExtractionOptions::default()
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
                ..ExtractionOptions::default()
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
                ..ExtractionOptions::default()
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
                ..ExtractionOptions::default()
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
                ..ExtractionOptions::default()
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

    #[test]
    fn hardlink_mode_outputs_image_folders_without_fallback() {
        let root = test_root("hardlink_mode_outputs_image_folders_without_fallback");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:hardlink");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                image_output_mode: ImageOutputMode::Hardlink,
                ..ExtractionOptions::default()
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 1);
        assert_eq!(summary.failed, 0);
        // 同一分区下硬链接应当成功，不应出现回退警告。
        assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
        let actual_output = actual_output_path(&summary);
        assert_xlsx_contains(&actual_output, &["artist:hardlink", "image1/"]);
        assert_duplicate_folder_contains(
            &actual_output.parent().unwrap().join("image1"),
            &["sample.png"],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skip_mode_writes_no_image_folders() {
        let root = test_root("skip_mode_writes_no_image_folders");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:skip-output");
        fs::write(input.join("broken.png"), b"not a png").unwrap();

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                image_output_mode: ImageOutputMode::Skip,
                ..ExtractionOptions::default()
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 2);
        assert_eq!(summary.failed, 1);
        let actual_output = actual_output_path(&summary);
        let actual_output_dir = actual_output.parent().unwrap();
        assert!(actual_output.exists());
        assert!(!actual_output_dir.join("image1").exists());
        assert!(!actual_output_dir.join("_Fail").exists());
        assert_xlsx_contains(&actual_output, &["artist:skip-output"]);
        assert_xlsx_lacks(&actual_output, "image1/");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_prunes_cache_for_removed_files() {
        let root = test_root("incremental_prunes_cache_for_removed_files");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let first_png = input.join("first.png");
        create_test_png(&first_png);
        insert_text_chunk(&first_png, "Description", "artist:keep");

        let second_png = input.join("second.png");
        create_colored_test_png(&second_png, [71, 128, 48]);
        insert_text_chunk(&second_png, "Description", "artist:remove");

        let output = root.join("metadata.xlsx");
        let options = ExtractionOptions {
            incremental: true,
            ..ExtractionOptions::default()
        };
        run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();

        let cache_space = single_subdir(&root.join(".novelai_metadata_cache"));
        assert_eq!(count_files_with_extension(&cache_space.join("records"), "json"), 2);
        assert_eq!(
            count_files_with_extension(&cache_space.join("thumbnails"), "png"),
            2
        );

        fs::remove_file(&second_png).unwrap();
        let second_summary =
            run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();
        assert_eq!(second_summary.total_png, 1);
        assert_eq!(second_summary.cache_hits, 1);

        assert_eq!(count_files_with_extension(&cache_space.join("records"), "json"), 1);
        assert_eq!(
            count_files_with_extension(&cache_space.join("thumbnails"), "png"),
            1
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clear_metadata_cache_removes_cache_dir() {
        let root = test_root("clear_metadata_cache_removes_cache_dir");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        let png_path = input.join("sample.png");
        create_test_png(&png_path);
        insert_text_chunk(&png_path, "Description", "artist:clear-cache");

        let output = root.join("metadata.xlsx");
        let options = ExtractionOptions {
            incremental: true,
            ..ExtractionOptions::default()
        };
        run_extraction_with_options(&input, &output, options, &NoopProgressSink).unwrap();
        assert!(root.join(".novelai_metadata_cache").is_dir());

        let summary = clear_metadata_cache_for_output(&output).unwrap();
        assert!(summary.existed);
        assert!(summary.removed_files > 0);
        assert!(!root.join(".novelai_metadata_cache").exists());

        let empty_summary = clear_metadata_cache_for_output(&output).unwrap();
        assert!(!empty_summary.existed);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dedupe_promotes_duplicate_when_representative_thumbnail_fails() {
        let root = test_root("dedupe_promotes_duplicate_when_representative_thumbnail_fails");
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();

        // 第一张图元数据有效但像素数据缺失：去重时会成为代表图，生成缩略图时失败。
        let broken_png = input.join("a_broken.png");
        create_metadata_only_png(&broken_png, "Description", "same prompt, artist:promo");

        let good_png = input.join("b_good.png");
        create_test_png(&good_png);
        insert_text_chunk(&good_png, "Description", "same prompt, artist:promo");

        let output = root.join("metadata.xlsx");
        let summary = run_extraction_with_options(
            &input,
            &output,
            ExtractionOptions {
                dedupe_positive_prompt: true,
                ..ExtractionOptions::default()
            },
            &NoopProgressSink,
        )
        .unwrap();

        assert_eq!(summary.total_png, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped_duplicates, 0);
        assert!(summary
            .warnings
            .iter()
            .any(|warning| warning.message.contains("无法创建缩略图")));

        let actual_output = actual_output_path(&summary);
        let actual_output_dir = actual_output.parent().unwrap();
        assert_xlsx_media_count(&actual_output, 1);
        assert_xlsx_contains(&actual_output, &["same prompt, artist:promo", "image1/"]);
        assert_duplicate_folder_contains(&actual_output_dir.join("image1"), &["b_good.png"]);
        assert_duplicate_folder_contains(&actual_output_dir.join("_Fail"), &["a_broken.png"]);

        fs::remove_dir_all(root).unwrap();
    }

    fn single_subdir(path: &Path) -> PathBuf {
        let subdirs = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|entry_path| entry_path.is_dir())
            .collect::<Vec<_>>();
        assert_eq!(subdirs.len(), 1, "{} should have one subdir", path.display());
        subdirs.into_iter().next().unwrap()
    }

    fn count_files_with_extension(dir: &Path, extension: &str) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case(extension))
            })
            .count()
    }

    fn assert_xlsx_lacks(output: &Path, unexpected_text: &str) {
        let file = File::open(output).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut shared_strings = String::new();
        archive
            .by_name("xl/sharedStrings.xml")
            .unwrap()
            .read_to_string(&mut shared_strings)
            .unwrap();
        assert!(
            !shared_strings.contains(unexpected_text),
            "xlsx should not contain {unexpected_text}"
        );
    }

    fn create_metadata_only_png(path: &Path, keyword: &str, text: &str) {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut data = Vec::new();
        data.extend(keyword.as_bytes());
        data.push(0);
        data.extend(text.as_bytes());
        bytes.extend(png_chunk(b"tEXt", &data));
        bytes.extend(png_chunk(b"IEND", &[]));
        fs::write(path, bytes).unwrap();
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
