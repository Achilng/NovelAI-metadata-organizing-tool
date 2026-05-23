# PROGRESS

## 当前状态
- 文件夹和 `.zip`/`.7z`/`.rar` 压缩包输入到 Excel 输出的主链路已跑通，进入端到端运行和打包验证。
- 已确认目标：开发一个 NovelAI 图像元数据整理工具，支持 GUI 输入、PNG 元数据提取和 Excel 导出。

## 已完成
- 创建项目进度文档。
- 创建 Tauri v2 + Vite + TypeScript 项目骨架。
- 接入 Tauri dialog 插件，用于前端选择输入路径和 `.xlsx` 输出路径。
- 创建主工具界面，包括输入、输出、处理状态、进度和警告区域。
- 接通 `extract_to_xlsx` Tauri 命令占位和前端调用链。
- 配置 Windows 图标占位资产，Rust 端 `cargo check` 可通过。
- 前端 `npm run build` 可通过。
- 实现 PNG `tEXt`、`zTXt`、`iTXt` 文本 chunk 读取器。
- 实现 NovelAI `Description`、`Comment.prompt`、`Comment.uc` 和 v4 caption 结构解析。
- 实现画师标签提取和去重规则。
- 添加 8 个后端单元测试覆盖 PNG 文本读取、提示词解析和画师标签提取。
- 实现文件夹递归扫描和单个 PNG 输入识别。
- 实现 160px PNG 缩略图生成，临时文件位于 `D:\Agent\Agent_temp\novelai_metadata_extractor`。
- 使用 `rust_xlsxwriter` 生成带图片缩略图、正向提示词、负向提示词和画师串的 `.xlsx`。
- 将真实文件夹处理流程接入 `extract_to_xlsx` Tauri 命令和前端进度事件。
- 添加文件夹到 XLSX 的后端样例测试。
- 实现 `.zip` 解压输入，解压目录位于 `D:\Agent\Agent_temp\novelai_metadata_extractor`。
- 实现 `.7z` 解压输入。
- 使用 `unrar-ng` 实现 `.rar` 解压输入，不依赖外部 7z 命令。
- 添加 ZIP、7z 和 RAR 到 XLSX 的后端样例测试。
- 修复并发运行时临时目录 ID 冲突风险。

## 进行中
- 端到端运行验证和 Windows 打包验证。

## 待办
- 添加端到端样例验证。
- 构建 Windows 桌面安装包。
- 记录打包产物和任何运行时注意事项。

## 记录
- 2026-05-23：创建 `PROGRESS.md`。
- 2026-05-23：完成可构建应用骨架；验证 `npm run build` 和 `cargo check` 通过。
- 2026-05-23：完成后端 PNG 文本元数据与 NovelAI 提示词解析核心；验证 `cargo test` 通过，8 个测试全部成功。
- 2026-05-23：完成文件夹输入到 XLSX 输出主链路；验证 `cargo test` 9 个测试通过，`cargo check` 和 `npm run build` 通过。
- 2026-05-23：完成 `.zip`、`.7z`、`.rar` 压缩包输入主链路；验证 `cargo test` 12 个测试通过，`cargo check` 和 `npm run build` 通过。
