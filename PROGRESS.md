# PROGRESS

## 当前状态
- 首版工具已完成核心功能和 Windows 打包验证。
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
- 强化 XLSX 样例测试，验证工作簿中包含嵌入图片和提示词文本。
- 配置 Windows MSI 语言为 `zh-CN`，解决中文产品名在 WiX 默认代码页下无法打包的问题。
- 完成 Tauri Windows release 构建，产物：
  - `src-tauri\target\release\bundle\msi\NovelAI 元数据整理工具_0.1.0_x64_zh-CN.msi`
  - `src-tauri\target\release\bundle\nsis\NovelAI 元数据整理工具_0.1.0_x64-setup.exe`
- 补充空文件夹、嵌套文件夹、非 NovelAI PNG、损坏 PNG 自动化验证。
- 修复 release 版启动时额外弹出命令行窗口的问题。

## 进行中
- 无。首版核心功能已完成。

## 后续建议
- 使用用户实际 NovelAI PNG / `.zip` / `.7z` / `.rar` 样例做人工验收。
- 视需要补充取消按钮、拖放输入和导出源路径等增强功能。

## 记录
- 2026-05-23：创建 `PROGRESS.md`。
- 2026-05-23：完成可构建应用骨架；验证 `npm run build` 和 `cargo check` 通过。
- 2026-05-23：完成后端 PNG 文本元数据与 NovelAI 提示词解析核心；验证 `cargo test` 通过，8 个测试全部成功。
- 2026-05-23：完成文件夹输入到 XLSX 输出主链路；验证 `cargo test` 9 个测试通过，`cargo check` 和 `npm run build` 通过。
- 2026-05-23：完成 `.zip`、`.7z`、`.rar` 压缩包输入主链路；验证 `cargo test` 12 个测试通过，`cargo check` 和 `npm run build` 通过。
- 2026-05-23：完成 Windows 打包验证；`npm run tauri:build` 成功生成 MSI 和 NSIS 安装包。
- 2026-05-23：补齐端到端异常场景自动化验证；`cargo test` 16 个测试全部通过。
- 2026-05-23：为 Windows release 入口添加 GUI 子系统声明，避免启动 `.exe` 时显示控制台窗口。
