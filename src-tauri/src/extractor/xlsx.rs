use anyhow::{Context, Result};
use rust_xlsxwriter::{Format, FormatAlign, Image, Workbook};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_EXCEL_TEXT_CHARS: usize = 32_767;
const THUMBNAIL_CELL_PIXELS: u32 = 176;

#[derive(Debug, Clone)]
pub struct WorkbookRow {
    pub thumbnail_path: PathBuf,
    pub source_path: String,
    pub sort_time: Option<SystemTime>,
    pub sort_time_text: String,
    pub positive_prompt: String,
    pub negative_prompt: String,
    pub artist_tags: Vec<String>,
    pub duplicate_folder: String,
}

pub fn write_xlsx(
    rows: &[WorkbookRow],
    output_path: &Path,
    include_time_column: bool,
) -> Result<()> {
    let mut workbook = Workbook::new();

    {
        let worksheet = workbook.add_worksheet();
        worksheet.set_name("NovelAI Metadata")?;

        let header_format = Format::new()
            .set_bold()
            .set_align(FormatAlign::Center)
            .set_align(FormatAlign::VerticalCenter);
        let text_format = Format::new().set_text_wrap().set_align(FormatAlign::Top);

        worksheet.set_freeze_panes(1, 0)?;
        let time_column = 1;
        let time_column_offset: u16 = if include_time_column { 1 } else { 0 };
        let positive_prompt_column = 1 + time_column_offset;
        let negative_prompt_column = 2 + time_column_offset;
        let artist_tags_column = 3 + time_column_offset;
        let duplicate_folder_column = 4 + time_column_offset;
        let source_path_column = 5 + time_column_offset;

        worksheet.set_column_width_pixels(0, THUMBNAIL_CELL_PIXELS)?;
        if include_time_column {
            worksheet.set_column_width(time_column, 20)?;
        }
        worksheet.set_column_width(positive_prompt_column, 64)?;
        worksheet.set_column_width(negative_prompt_column, 48)?;
        worksheet.set_column_width(artist_tags_column, 34)?;
        worksheet.set_column_width(duplicate_folder_column, 18)?;
        worksheet.set_column_width(source_path_column, 58)?;
        worksheet.set_row_height_pixels(0, 28)?;

        worksheet.write_string_with_format(0, 0, "图片", &header_format)?;
        if include_time_column {
            worksheet.write_string_with_format(0, time_column, "时间", &header_format)?;
        }
        worksheet.write_string_with_format(
            0,
            positive_prompt_column,
            "正向提示词",
            &header_format,
        )?;
        worksheet.write_string_with_format(
            0,
            negative_prompt_column,
            "负向提示词",
            &header_format,
        )?;
        worksheet.write_string_with_format(0, artist_tags_column, "画师串", &header_format)?;
        worksheet.write_string_with_format(
            0,
            duplicate_folder_column,
            "重复文件夹",
            &header_format,
        )?;
        worksheet.write_string_with_format(0, source_path_column, "图片路径", &header_format)?;

        for (index, row) in rows.iter().enumerate() {
            let row_number = (index + 1) as u32;
            worksheet.set_row_height_pixels(row_number, THUMBNAIL_CELL_PIXELS)?;

            let image = Image::new(&row.thumbnail_path)
                .with_context(|| format!("无法读取缩略图：{}", row.thumbnail_path.display()))?
                .set_alt_text(format!("图片：{}", row.source_path));
            worksheet.insert_image_fit_to_cell_centered(row_number, 0, &image)?;

            if include_time_column {
                worksheet.write_string_with_format(
                    row_number,
                    time_column,
                    truncate_for_excel(&row.sort_time_text),
                    &text_format,
                )?;
            }
            worksheet.write_string_with_format(
                row_number,
                positive_prompt_column,
                truncate_for_excel(&row.positive_prompt),
                &text_format,
            )?;
            worksheet.write_string_with_format(
                row_number,
                negative_prompt_column,
                truncate_for_excel(&row.negative_prompt),
                &text_format,
            )?;
            worksheet.write_string_with_format(
                row_number,
                artist_tags_column,
                truncate_for_excel(&row.artist_tags.join("\n")),
                &text_format,
            )?;
            if !row.duplicate_folder.is_empty() {
                worksheet.write_string_with_format(
                    row_number,
                    duplicate_folder_column,
                    truncate_for_excel(&row.duplicate_folder),
                    &text_format,
                )?;
            }
            worksheet.write_string_with_format(
                row_number,
                source_path_column,
                truncate_for_excel(&row.source_path),
                &text_format,
            )?;
        }
    }

    workbook
        .save(output_path)
        .with_context(|| format!("无法保存 Excel 文件：{}", output_path.display()))?;
    Ok(())
}

fn truncate_for_excel(value: &str) -> String {
    value.chars().take(MAX_EXCEL_TEXT_CHARS).collect()
}
