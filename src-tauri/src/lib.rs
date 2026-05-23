use serde::Serialize;
use std::path::Path;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
pub struct FileWarning {
    path: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    total_png: usize,
    processed: usize,
    failed: usize,
    output_path: String,
    warnings: Vec<FileWarning>,
}

#[derive(Debug, Clone, Serialize)]
struct ProgressPayload {
    total_png: Option<usize>,
    processed: Option<usize>,
    failed: Option<usize>,
    current_file: Option<String>,
    message: Option<String>,
}

#[tauri::command]
async fn extract_to_xlsx(
    app: AppHandle,
    input_path: String,
    output_path: String,
) -> Result<RunSummary, String> {
    let input_path_for_task = input_path.clone();
    let output_path_for_task = output_path.clone();
    let app_for_task = app.clone();

    tauri::async_runtime::spawn_blocking(move || {
        validate_paths(&input_path_for_task, &output_path_for_task)?;
        emit_status(
            &app_for_task,
            "extract:start",
            ProgressPayload {
                total_png: Some(0),
                processed: Some(0),
                failed: Some(0),
                current_file: None,
                message: Some("基础应用骨架已接通，核心提取流程将在下一步实现。".to_string()),
            },
        )?;

        Ok(RunSummary {
            total_png: 0,
            processed: 0,
            failed: 0,
            output_path: output_path_for_task,
            warnings: vec![FileWarning {
                path: input_path_for_task,
                message: "核心提取流程尚未实现。".to_string(),
            }],
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

fn validate_paths(input_path: &str, output_path: &str) -> Result<(), String> {
    let input = Path::new(input_path);
    if !input.exists() {
        return Err("输入路径不存在。".to_string());
    }

    let output = Path::new(output_path);
    if output.extension().and_then(|value| value.to_str()) != Some("xlsx") {
        return Err("输出路径必须是 .xlsx 文件。".to_string());
    }

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err("输出目录不存在。".to_string());
        }
    }

    Ok(())
}

fn emit_status(app: &AppHandle, event_name: &str, payload: ProgressPayload) -> Result<(), String> {
    app.emit(event_name, payload)
        .map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![extract_to_xlsx])
        .run(tauri::generate_context!())
        .expect("failed to run app");
}
