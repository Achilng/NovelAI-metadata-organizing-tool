use anyhow::{bail, Context, Result};
use calamine::{open_workbook, Data, Range, Reader, Xlsx};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const POSITIVE_PROMPT_HEADER: &str = "正向提示词";
const NEGATIVE_PROMPT_HEADER: &str = "负向提示词";
const PREVIEW_LIMIT: usize = 3;
const PROGRESS_INTERVAL: usize = 250;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversionPreviewItem {
    pub fixed_prompt: String,
    pub negative_prompt: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct XlsxInspection {
    pub record_count: usize,
    pub preview: Vec<ConversionPreviewItem>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversionProgress {
    pub total: usize,
    pub processed: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConversionSummary {
    pub exported: usize,
    pub output_path: String,
}

#[derive(Debug, Clone, Copy)]
struct SheetLayout {
    header_row: usize,
    positive_prompt_column: usize,
    negative_prompt_column: usize,
}

pub fn inspect_xlsx_file(input_path: &Path) -> Result<XlsxInspection> {
    validate_input_path(input_path)?;
    let range = read_first_sheet(input_path)?;
    let layout = find_sheet_layout(&range)?;

    let mut record_count = 0;
    let mut preview = Vec::with_capacity(PREVIEW_LIMIT);
    for row in data_rows(&range, layout.header_row) {
        if row_is_empty(row) {
            continue;
        }

        record_count += 1;
        if preview.len() < PREVIEW_LIMIT {
            preview.push(preview_item(row, layout));
        }
    }

    Ok(XlsxInspection {
        record_count,
        preview,
    })
}

pub fn convert_xlsx_file(
    input_path: &Path,
    output_path: &Path,
    mut on_progress: impl FnMut(ConversionProgress),
) -> Result<ConversionSummary> {
    validate_input_path(input_path)?;
    validate_output_path(input_path, output_path)?;

    let range = read_first_sheet(input_path)?;
    let layout = find_sheet_layout(&range)?;
    let total = data_rows(&range, layout.header_row)
        .filter(|row| !row_is_empty(row))
        .count();

    on_progress(ConversionProgress {
        total,
        processed: 0,
        message: format!("准备转换 {total} 条记录..."),
    });

    let temp_path = unique_sibling_path(output_path, "tmp");
    let mut temp_guard = TemporaryFile::new(temp_path.clone());
    let file = File::create(&temp_path)
        .with_context(|| format!("无法创建临时 JSON 文件：{}", temp_path.display()))?;
    let mut writer = BufWriter::new(file);

    writer.write_all(b"{\n  \"presets\": {")?;
    let mut exported = 0;
    for row in data_rows(&range, layout.header_row) {
        if row_is_empty(row) {
            continue;
        }

        exported += 1;
        let item = preview_item(row, layout);
        if exported > 1 {
            writer.write_all(b",")?;
        }
        writer.write_all(b"\n    ")?;
        write_json_string(&mut writer, &exported.to_string())?;
        writer.write_all(b": {\n      \"fixedPrompt\": ")?;
        write_json_string(&mut writer, &item.fixed_prompt)?;
        writer.write_all(b",\n      \"fixedPrompt_end\": \"\",\n      \"negativePrompt\": ")?;
        write_json_string(&mut writer, &item.negative_prompt)?;
        writer.write_all(b"\n    }")?;

        if exported % PROGRESS_INTERVAL == 0 || exported == total {
            on_progress(ConversionProgress {
                total,
                processed: exported,
                message: format!("正在转换 {exported} / {total}..."),
            });
        }
    }
    writer.write_all(b"\n  },\n  \"images\": {}\n}\n")?;
    writer.flush().context("无法写完 JSON 文件")?;
    writer
        .get_ref()
        .sync_all()
        .context("无法同步 JSON 文件到磁盘")?;
    drop(writer);

    replace_output_file(&temp_path, output_path)?;
    temp_guard.commit();

    on_progress(ConversionProgress {
        total,
        processed: exported,
        message: format!("转换完成，共导出 {exported} 条记录。"),
    });

    Ok(ConversionSummary {
        exported,
        output_path: output_path.display().to_string(),
    })
}

fn validate_input_path(input_path: &Path) -> Result<()> {
    if !input_path.is_file() {
        bail!("请选择有效的 XLSX 文件。");
    }
    if !has_extension(input_path, "xlsx") {
        bail!("输入文件必须是 .xlsx 文件。");
    }
    Ok(())
}

fn validate_output_path(input_path: &Path, output_path: &Path) -> Result<()> {
    if !has_extension(output_path, "json") {
        bail!("输出路径必须是 .json 文件。");
    }
    if canonical_or_original(input_path) == canonical_or_original(output_path) {
        bail!("输入和输出路径不能相同。");
    }
    let parent = output_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("输出路径缺少父目录。")?;
    if !parent.is_dir() {
        bail!("输出目录不存在：{}", parent.display());
    }
    Ok(())
}

fn read_first_sheet(input_path: &Path) -> Result<Range<Data>> {
    let mut workbook: Xlsx<_> = open_workbook(input_path)
        .with_context(|| format!("无法读取 XLSX 文件：{}", input_path.display()))?;
    let sheet_name = workbook
        .sheet_names()
        .first()
        .cloned()
        .context("XLSX 中没有工作表。")?;
    workbook
        .worksheet_range(&sheet_name)
        .with_context(|| format!("无法读取工作表：{sheet_name}"))
}

fn find_sheet_layout(range: &Range<Data>) -> Result<SheetLayout> {
    for (header_row, row) in range.rows().enumerate() {
        let positive_prompt_column = find_header_column(row, POSITIVE_PROMPT_HEADER);
        let negative_prompt_column = find_header_column(row, NEGATIVE_PROMPT_HEADER);
        if let (Some(positive_prompt_column), Some(negative_prompt_column)) =
            (positive_prompt_column, negative_prompt_column)
        {
            return Ok(SheetLayout {
                header_row,
                positive_prompt_column,
                negative_prompt_column,
            });
        }
    }

    bail!("找不到“正向提示词”和“负向提示词”列，请选择本工具生成的 XLSX 文件。")
}

fn find_header_column(row: &[Data], expected: &str) -> Option<usize> {
    row.iter()
        .position(|cell| cell_text(cell).trim() == expected)
}

fn data_rows(range: &Range<Data>, header_row: usize) -> impl Iterator<Item = &[Data]> {
    range.rows().skip(header_row + 1)
}

fn row_is_empty(row: &[Data]) -> bool {
    row.iter().all(|cell| cell_text(cell).trim().is_empty())
}

fn preview_item(row: &[Data], layout: SheetLayout) -> ConversionPreviewItem {
    ConversionPreviewItem {
        fixed_prompt: row
            .get(layout.positive_prompt_column)
            .map(cell_text)
            .unwrap_or_default(),
        negative_prompt: row
            .get(layout.negative_prompt_column)
            .map(cell_text)
            .unwrap_or_default(),
    }
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(value) => value.clone(),
        _ => cell.to_string(),
    }
}

fn write_json_string(writer: &mut impl Write, value: &str) -> Result<()> {
    serde_json::to_writer(writer, value).context("无法序列化 JSON 文本")
}

fn replace_output_file(temp_path: &Path, output_path: &Path) -> Result<()> {
    let backup_path = unique_sibling_path(output_path, "backup");
    let had_existing_output = output_path.exists();

    if had_existing_output {
        fs::rename(output_path, &backup_path)
            .with_context(|| format!("无法暂存已有 JSON 文件：{}", output_path.display()))?;
    }

    if let Err(error) = fs::rename(temp_path, output_path) {
        if had_existing_output {
            let _ = fs::rename(&backup_path, output_path);
        }
        return Err(error)
            .with_context(|| format!("无法保存 JSON 文件：{}", output_path.display()));
    }

    if had_existing_output {
        fs::remove_file(&backup_path)
            .with_context(|| format!("无法清理旧 JSON 备份：{}", backup_path.display()))?;
    }
    Ok(())
}

fn unique_sibling_path(output_path: &Path, suffix: &str) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = output_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    output_path.with_file_name(format!(
        ".{file_name}.{}.{}.{}",
        std::process::id(),
        counter,
        suffix
    ))
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

struct TemporaryFile {
    path: PathBuf,
    committed: bool,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{convert_xlsx_file, inspect_xlsx_file};
    use rust_xlsxwriter::Workbook;
    use serde_json::Value;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn inspects_and_converts_rows_in_workbook_order() {
        let root = test_root("standard");
        let input = root.join("metadata.xlsx");
        let output = root.join("metadata.json");
        let mut workbook = Workbook::new();
        let worksheet = workbook.add_worksheet();
        worksheet.write_string(0, 0, "图片").unwrap();
        worksheet.write_string(0, 1, "正向提示词").unwrap();
        worksheet.write_string(0, 2, "负向提示词").unwrap();
        worksheet.write_string(0, 3, "图片路径").unwrap();
        worksheet
            .write_string(1, 1, "第一行\n\"引号\"与中文")
            .unwrap();
        worksheet.write_string(1, 2, "负向一").unwrap();
        worksheet.write_string(1, 3, "first.png").unwrap();
        worksheet.write_string(3, 3, "empty-prompts.png").unwrap();
        workbook.save(&input).unwrap();

        let inspection = inspect_xlsx_file(&input).unwrap();
        assert_eq!(inspection.record_count, 2);
        assert_eq!(inspection.preview.len(), 2);
        assert_eq!(inspection.preview[0].fixed_prompt, "第一行\n\"引号\"与中文");
        assert_eq!(inspection.preview[1].fixed_prompt, "");

        let mut progress = Vec::new();
        let summary = convert_xlsx_file(&input, &output, |event| progress.push(event)).unwrap();
        assert_eq!(summary.exported, 2);
        assert_eq!(progress.last().unwrap().processed, 2);

        let json: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(
            json["presets"]["1"]["fixedPrompt"],
            "第一行\n\"引号\"与中文"
        );
        assert_eq!(json["presets"]["1"]["fixedPrompt_end"], "");
        assert_eq!(json["presets"]["1"]["negativePrompt"], "负向一");
        assert_eq!(json["presets"]["2"]["fixedPrompt"], "");
        assert_eq!(json["images"], serde_json::json!({}));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_prompt_columns_when_time_column_exists() {
        let root = test_root("time-column");
        let input = root.join("metadata.xlsx");
        let mut workbook = Workbook::new();
        let worksheet = workbook.add_worksheet();
        worksheet.write_string(0, 0, "图片").unwrap();
        worksheet.write_string(0, 1, "时间").unwrap();
        worksheet.write_string(0, 2, "正向提示词").unwrap();
        worksheet.write_string(0, 3, "负向提示词").unwrap();
        worksheet.write_string(1, 2, "positive").unwrap();
        worksheet.write_string(1, 3, "negative").unwrap();
        workbook.save(&input).unwrap();

        let inspection = inspect_xlsx_file(&input).unwrap();
        assert_eq!(inspection.record_count, 1);
        assert_eq!(inspection.preview[0].fixed_prompt, "positive");
        assert_eq!(inspection.preview[0].negative_prompt, "negative");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_workbook_without_required_headers() {
        let root = test_root("missing-headers");
        let input = root.join("metadata.xlsx");
        let mut workbook = Workbook::new();
        workbook
            .add_worksheet()
            .write_string(0, 0, "其他列")
            .unwrap();
        workbook.save(&input).unwrap();

        let error = inspect_xlsx_file(&input).unwrap_err().to_string();
        assert!(error.contains("找不到"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_damaged_xlsx() {
        let root = test_root("damaged");
        let input = root.join("metadata.xlsx");
        fs::write(&input, b"not an xlsx file").unwrap();

        assert!(inspect_xlsx_file(&input).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_contiguous_numbering_for_large_batches() {
        let root = test_root("large-batch");
        let input = root.join("metadata.xlsx");
        let output = root.join("metadata.json");
        let mut workbook = Workbook::new();
        let worksheet = workbook.add_worksheet();
        worksheet.write_string(0, 0, "正向提示词").unwrap();
        worksheet.write_string(0, 1, "负向提示词").unwrap();
        for index in 1..=1_000_u32 {
            worksheet
                .write_string(index, 0, format!("positive-{index}"))
                .unwrap();
            worksheet
                .write_string(index, 1, format!("negative-{index}"))
                .unwrap();
        }
        workbook.save(&input).unwrap();

        let summary = convert_xlsx_file(&input, &output, |_| {}).unwrap();
        assert_eq!(summary.exported, 1_000);
        let json: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(json["presets"].as_object().unwrap().len(), 1_000);
        assert_eq!(json["presets"]["1000"]["fixedPrompt"], "positive-1000");

        fs::remove_dir_all(root).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = Path::new(r"D:\Agent\Agent_temp")
            .join("novelai_metadata_extractor_tests")
            .join(format!("converter-{name}-{}-{counter}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
