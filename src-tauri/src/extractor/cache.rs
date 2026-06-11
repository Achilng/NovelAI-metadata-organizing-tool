use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CACHE_DIR_NAME: &str = ".novelai_metadata_cache";
const CACHE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub size: u64,
    pub modified_nanos: Option<u128>,
}

impl FileFingerprint {
    pub fn from_path(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("Failed to read file info: {}", path.display()))?;

        Ok(Self {
            size: metadata.len(),
            modified_nanos: metadata.modified().ok().and_then(system_time_to_nanos),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheManifest {
    version: u32,
    input_identity: String,
    archive_fingerprint: Option<FileFingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedImageRecord {
    pub display_path: String,
    pub source_size: u64,
    pub source_modified_nanos: Option<u128>,
    pub positive_prompt: String,
    pub negative_prompt: String,
    pub artist_tags: Vec<String>,
    pub metadata_warning: Option<String>,
    pub metadata_failed: bool,
    pub thumbnail_file_name: Option<String>,
    pub thumbnail_error: Option<String>,
}

pub struct CacheStore {
    records_dir: PathBuf,
    thumbnails_dir: PathBuf,
    records: HashMap<String, CachedImageRecord>,
}

impl CacheStore {
    pub fn open(
        output_parent: &Path,
        input_path: &Path,
        archive_fingerprint: Option<FileFingerprint>,
    ) -> Result<Self> {
        let input_identity = input_identity(input_path);
        let cache_key = cache_key(&input_identity, archive_fingerprint);
        let root = output_parent.join(CACHE_DIR_NAME).join(cache_key);
        let records_dir = root.join("records");
        let thumbnails_dir = root.join("thumbnails");

        fs::create_dir_all(&records_dir).with_context(|| {
            format!(
                "Failed to create cache records dir: {}",
                records_dir.display()
            )
        })?;
        fs::create_dir_all(&thumbnails_dir).with_context(|| {
            format!(
                "Failed to create cache thumbnails dir: {}",
                thumbnails_dir.display()
            )
        })?;

        let manifest = CacheManifest {
            version: CACHE_SCHEMA_VERSION,
            input_identity,
            archive_fingerprint,
        };
        let manifest_json =
            serde_json::to_vec_pretty(&manifest).context("Failed to serialize cache manifest")?;
        fs::write(root.join("manifest.json"), manifest_json)
            .with_context(|| format!("Failed to write cache manifest: {}", root.display()))?;

        let records = load_records(&records_dir)?;

        Ok(Self {
            records_dir,
            thumbnails_dir,
            records,
        })
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn get(&self, display_path: &str) -> Option<&CachedImageRecord> {
        self.records.get(display_path)
    }

    pub fn save_record(&mut self, record: CachedImageRecord) -> Result<()> {
        self.save_record_file(&record)?;
        self.records.insert(record.display_path.clone(), record);
        Ok(())
    }

    pub fn save_record_file(&self, record: &CachedImageRecord) -> Result<()> {
        let record_path = self.record_path(&record.display_path);
        let record_json = serde_json::to_vec(record).context("Failed to serialize cache record")?;
        fs::write(&record_path, record_json)
            .with_context(|| format!("Failed to write cache record: {}", record_path.display()))?;
        Ok(())
    }

    pub fn thumbnail_file_name(&self, display_path: &str) -> String {
        format!("{}.png", stable_hash_hex(display_path))
    }

    pub fn thumbnail_path_for_file_name(&self, file_name: &str) -> PathBuf {
        self.thumbnails_dir.join(file_name)
    }

    pub fn thumbnail_path_for_display_path(&self, display_path: &str) -> PathBuf {
        self.thumbnail_path_for_file_name(&self.thumbnail_file_name(display_path))
    }

    fn record_path(&self, display_path: &str) -> PathBuf {
        self.records_dir
            .join(format!("{}.json", stable_hash_hex(display_path)))
    }

    /// 删除当前扫描中已不存在的图片对应的缓存记录和缩略图，返回删除的文件数量。
    ///
    /// 以磁盘内容为准（而不是内存中的记录表），这样并行写入的新记录不会被误删，
    /// 中断运行残留的孤儿缩略图也能一并清掉。
    pub fn prune_missing(&mut self, current_display_paths: &HashSet<String>) -> Result<usize> {
        let expected_stems: HashSet<String> = current_display_paths
            .iter()
            .map(|display_path| stable_hash_hex(display_path))
            .collect();

        let mut removed = 0_usize;
        removed += remove_unexpected_files(&self.records_dir, "json", &expected_stems)?;
        removed += remove_unexpected_files(&self.thumbnails_dir, "png", &expected_stems)?;

        self.records
            .retain(|display_path, _| current_display_paths.contains(display_path));

        Ok(removed)
    }
}

fn remove_unexpected_files(
    dir: &Path,
    extension: &str,
    expected_stems: &HashSet<String>,
) -> Result<usize> {
    let mut removed = 0_usize;

    for entry in
        fs::read_dir(dir).with_context(|| format!("无法读取缓存目录：{}", dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("无法读取缓存目录条目：{}", dir.display()))?;
        if !entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false)
        {
            continue;
        }

        let path = entry.path();
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            continue;
        }

        let stem_matches = path
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|stem| expected_stems.contains(stem));
        if stem_matches {
            continue;
        }

        fs::remove_file(&path)
            .with_context(|| format!("无法删除失效缓存文件：{}", path.display()))?;
        removed += 1;
    }

    Ok(removed)
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheClearSummary {
    pub existed: bool,
    pub removed_files: usize,
    pub freed_bytes: u64,
}

/// 删除输出路径同级的整个 `.novelai_metadata_cache` 目录。
pub fn clear_cache_root(output_parent: &Path) -> Result<CacheClearSummary> {
    let cache_root = output_parent.join(CACHE_DIR_NAME);
    if !cache_root.is_dir() {
        return Ok(CacheClearSummary {
            existed: false,
            removed_files: 0,
            freed_bytes: 0,
        });
    }

    let mut removed_files = 0_usize;
    let mut freed_bytes = 0_u64;
    for entry in walkdir::WalkDir::new(&cache_root) {
        let entry =
            entry.with_context(|| format!("无法统计缓存目录：{}", cache_root.display()))?;
        if entry.file_type().is_file() {
            removed_files += 1;
            freed_bytes += entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        }
    }

    fs::remove_dir_all(&cache_root)
        .with_context(|| format!("无法删除缓存目录：{}", cache_root.display()))?;

    Ok(CacheClearSummary {
        existed: true,
        removed_files,
        freed_bytes,
    })
}

pub fn system_time_to_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

fn input_identity(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn cache_key(input_identity: &str, archive_fingerprint: Option<FileFingerprint>) -> String {
    let archive_part = archive_fingerprint
        .map(|fingerprint| format!("{}:{:?}", fingerprint.size, fingerprint.modified_nanos))
        .unwrap_or_else(|| "folder-or-png".to_string());
    stable_hash_hex(&format!(
        "v{CACHE_SCHEMA_VERSION}\n{input_identity}\n{archive_part}"
    ))
}

fn load_records(records_dir: &Path) -> Result<HashMap<String, CachedImageRecord>> {
    let mut records = HashMap::new();

    for entry in fs::read_dir(records_dir)
        .with_context(|| format!("Failed to read cache dir: {}", records_dir.display()))?
    {
        let entry = entry.with_context(|| {
            format!("Failed to read cache dir entry: {}", records_dir.display())
        })?;
        if !entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false)
        {
            continue;
        }

        let path = entry.path();
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            continue;
        }

        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<CachedImageRecord>(&bytes) else {
            continue;
        };
        records.insert(record.display_path.clone(), record);
    }

    Ok(records)
}

fn stable_hash_hex(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
