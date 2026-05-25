use std::path::Path;
use tauri::{AppHandle, Emitter};

mod extractor;

use extractor::{
    run_extraction_with_options, ExtractionOptions, FileWarning, ProgressPayload, ProgressSink,
    RunSummary,
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
) -> Result<RunSummary, String> {
    let app_for_task = app.clone();

    tauri::async_runtime::spawn_blocking(move || {
        let sink = TauriProgressSink { app: app_for_task };
        let options = ExtractionOptions {
            dedupe_positive_prompt,
            dedupe_artist_tags,
            sort_by_time,
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![extract_to_xlsx])
        .run(tauri::generate_context!())
        .expect("failed to run app");
}
