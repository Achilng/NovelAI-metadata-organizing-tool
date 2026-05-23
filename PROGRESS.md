# PROGRESS

## 当前状态
- 应用骨架阶段完成，进入后端核心功能开发。
- 已确认目标：开发一个 NovelAI 图像元数据整理工具，支持 GUI 输入、PNG 元数据提取和 Excel 导出。

## 已完成
- 创建项目进度文档。
- 创建 Tauri v2 + Vite + TypeScript 项目骨架。
- 接入 Tauri dialog 插件，用于前端选择输入路径和 `.xlsx` 输出路径。
- 创建主工具界面，包括输入、输出、处理状态、进度和警告区域。
- 接通 `extract_to_xlsx` Tauri 命令占位和前端调用链。
- 配置 Windows 图标占位资产，Rust 端 `cargo check` 可通过。
- 前端 `npm run build` 可通过。

## 进行中
- 后端核心提取流程开发。

## 待办
- 实现 PNG 文本 chunk 读取器。
- 实现 NovelAI 元数据解析、正向提示词提取、负向提示词提取和画师标签提取。
- 实现文件夹递归扫描和压缩包输入处理。
- 实现缩略图创建和 XLSX 写入。
- 将真实提取流程接入 `extract_to_xlsx` 命令。
- 添加端到端样例验证。

## 记录
- 2026-05-23：创建 `PROGRESS.md`。
- 2026-05-23：完成可构建应用骨架；验证 `npm run build` 和 `cargo check` 通过。
