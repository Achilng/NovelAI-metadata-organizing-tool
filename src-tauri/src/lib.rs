use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};

mod extractor;

use extractor::{
    clear_metadata_cache_for_output, convert_xlsx_file, dedupe_zhihuiji_json_file,
    inspect_xlsx_file, inspect_zhihuiji_json_file, run_extraction_with_options, CacheClearSummary,
    ConversionProgress, ConversionSummary, ExtractionOptions, FileWarning, ImageOutputMode,
    JsonDedupeInspection, JsonDedupeProgress, JsonDedupeSummary, ProgressPayload, ProgressSink,
    RunSummary, XlsxInspection,
};

struct TauriProgressSink {
    app: AppHandle,
}

impl ProgressSink for TauriProgressSink {
    fn emit_progress(&self, event_name: &str, payload: ProgressPayload) {
        let _ = self.app.emit(event_name, payload);
    }

    fn emit_warning(&self, warning: &FileWarning) {
        let _ = self.app.emit("extract:file_warning", warning);
    }
}

#[tauri::command]
async fn extract_to_xlsx(
    app: AppHandle,
    input_path: String,
    output_path: String,
    dedupe_positive_prompt: bool,
    dedupe_artist_tags: bool,
    sort_by_time: bool,
    incremental: bool,
    image_output_mode: String,
) -> Result<RunSummary, String> {
    let app_for_task = app.clone();
    let image_output_mode = ImageOutputMode::parse(&image_output_mode)
        .ok_or_else(|| format!("未知的图片输出方式：{image_output_mode}"))?;

    tauri::async_runtime::spawn_blocking(move || {
        let sink = TauriProgressSink { app: app_for_task };
        let options = ExtractionOptions {
            dedupe_positive_prompt,
            dedupe_artist_tags,
            sort_by_time,
            incremental,
            image_output_mode,
        };
        run_extraction_with_options(
            Path::new(&input_path),
            Path::new(&output_path),
            options,
            &sink,
        )
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn clear_metadata_cache(output_path: String) -> Result<CacheClearSummary, String> {
    tauri::async_runtime::spawn_blocking(move || {
        clear_metadata_cache_for_output(Path::new(&output_path))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn inspect_xlsx(input_path: String) -> Result<XlsxInspection, String> {
    tauri::async_runtime::spawn_blocking(move || inspect_xlsx_file(Path::new(&input_path)))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn convert_xlsx_to_zhihuiji_json(
    app: AppHandle,
    input_path: String,
    output_path: String,
) -> Result<ConversionSummary, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        convert_xlsx_file(
            Path::new(&input_path),
            Path::new(&output_path),
            |payload: ConversionProgress| {
                let _ = app_for_task.emit("convert:progress", payload);
            },
        )
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn inspect_zhihuiji_json(input_path: String) -> Result<JsonDedupeInspection, String> {
    tauri::async_runtime::spawn_blocking(move || inspect_zhihuiji_json_file(Path::new(&input_path)))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn dedupe_zhihuiji_json(
    app: AppHandle,
    input_path: String,
    output_path: String,
) -> Result<JsonDedupeSummary, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        dedupe_zhihuiji_json_file(
            Path::new(&input_path),
            Path::new(&output_path),
            |payload: JsonDedupeProgress| {
                let _ = app_for_task.emit("json-dedupe:progress", payload);
            },
        )
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn open_output_folder(path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let folder = if path.is_dir() {
        path
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "无法确定输出文件夹。".to_string())?
    };
    if !folder.is_dir() {
        return Err(format!("输出文件夹不存在：{}", folder.display()));
    }

    std::process::Command::new("explorer.exe")
        .arg(folder)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开输出文件夹：{error}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            extract_to_xlsx,
            clear_metadata_cache,
            inspect_xlsx,
            convert_xlsx_to_zhihuiji_json,
            inspect_zhihuiji_json,
            dedupe_zhihuiji_json,
            open_output_folder
        ])
        .run(tauri::generate_context!())
        .expect("failed to run app");
}
