# PROGRESS

## 当前状态
- 首版工具已完成核心功能、Windows 打包验证、导出去重开关、重复图片目录整理、可选按时间排序和默认启用的增量整理。
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
- 新增导出去重开关：
  - 正面提示词去重。
  - 画师串去重。
  - UI 显示去重跳过数量，后端 `RunSummary` 返回 `skipped_duplicates`。
- 补充正面提示词去重和画师串去重的自动化测试，验证 XLSX 实际嵌入图片数量。
- 扩展图片文件夹输出：
  - 为每个成功写入 XLSX 的图片行创建 `image1`、`image2` 等自动编号文件夹。
  - 将重复组代表图和后续重复图都复制到代表图对应文件夹，不移动原始输入文件。
  - XLSX 使用“图片文件夹”列记录 `image1/` 等目录名，单图也会填写。
- 补充图片文件夹自动化测试，覆盖单图、正面提示词去重、画师串去重、多个重复组和去重开启时的唯一图。
- 新增输出防呆：
  - 根据用户选择的 `.xlsx` 文件名创建同名输出包文件夹，再把实际 `.xlsx` 和重复图片目录放进去。
  - 同名输出包文件夹已存在时自动追加 `_1`、`_2` 等编号，避免覆盖或混入旧输出。
- 新增失败图片集中输出：
  - 处理失败的图片会复制到输出包文件夹内的 `_Fail` 文件夹。
  - 没有失败图片时不创建 `_Fail` 文件夹。
- 新增按时间升序整理开关：
  - 当前元数据解析结果没有可用于绝对排序的生成日期字段，因此使用图片文件创建时间。
  - 启用后 XLSX 增加“时间”列，并按时间升序排序，最近图片排在最后。
  - 无法取得时间的图片排在最后，时间列留空。
  - 图片文件夹按文件夹内图片最早时间添加前缀，例如 `2026-05-26_103000_image1`。
  - 无法取得时间的文件夹使用 `9999-12-31_235959_` 前缀，以便资源管理器名称排序时靠后。
- 新增默认启用的增量整理：
  - 在输出路径同级 `.novelai_metadata_cache` 中保存解析结果和持久缩略图。
  - 文件夹和单 PNG 输入按相对路径、文件大小和修改时间复用缓存。
  - 压缩包输入按压缩包路径、大小和修改时间建立缓存空间；压缩包未变时可复用解压后图片的解析结果。
  - 每张新增或变更图片处理完成后写入独立缓存记录，中断后下次运行可复用已完成部分。
  - XLSX、去重目录和失败目录仍按当前选项重新生成，避免复用旧导出造成错位。
  - UI 显示缓存复用数量和新处理数量。
- 新增 XLSX 最后一列“图片路径”：
  - 文件夹和单 PNG 输入记录原图路径。
  - 压缩包输入记录“压缩包路径 > 包内路径”，避免使用运行后会清理的临时解压路径。
- 新增后端多线程图片处理：
  - 未启用去重时，并行完成每张 PNG 的缓存判断、元数据读取和缩略图生成。
  - 启用去重时，并行预读元数据，再按扫描顺序合并，保持去重结果和输出编号稳定。
  - 工作线程数最多 8 个，并受当前机器可用并行度和 PNG 数量限制。

## 进行中
- 无。首版核心功能已完成。

## 后续建议
- 使用用户实际 NovelAI PNG / `.zip` / `.7z` / `.rar` 样例做人工验收。
- 视需要补充取消按钮、拖放输入和缓存清理入口等增强功能。

## 记录
- 2026-05-23：创建 `PROGRESS.md`。
- 2026-05-23：完成可构建应用骨架；验证 `npm run build` 和 `cargo check` 通过。
- 2026-05-23：完成后端 PNG 文本元数据与 NovelAI 提示词解析核心；验证 `cargo test` 通过，8 个测试全部成功。
- 2026-05-23：完成文件夹输入到 XLSX 输出主链路；验证 `cargo test` 9 个测试通过，`cargo check` 和 `npm run build` 通过。
- 2026-05-23：完成 `.zip`、`.7z`、`.rar` 压缩包输入主链路；验证 `cargo test` 12 个测试通过，`cargo check` 和 `npm run build` 通过。
- 2026-05-23：完成 Windows 打包验证；`npm run tauri:build` 成功生成 MSI 和 NSIS 安装包。
- 2026-05-23：补齐端到端异常场景自动化验证；`cargo test` 16 个测试全部通过。
- 2026-05-23：为 Windows release 入口添加 GUI 子系统声明，避免启动 `.exe` 时显示控制台窗口。
- 2026-05-24：新增正面提示词/画师串导出去重开关；验证 `cargo test` 18 个测试全部通过，`npm.cmd run build` 和 `npm.cmd run tauri:build` 通过。
- 2026-05-24：新增重复图片自动编号目录输出和 XLSX“重复文件夹”列；验证 `cargo test` 19 个测试全部通过，`npm.cmd run build` 通过。
- 2026-05-24：将输出按钮文案从“选择 .xlsx 保存路径”调整为“选择输出路径”；验证 `npm.cmd run tauri:build` 通过。
- 2026-05-24：新增输出包文件夹防呆设计；验证 `cargo test` 20 个测试全部通过，`npm.cmd run tauri:build` 通过。
- 2026-05-24：新增失败图片 `_Fail` 文件夹集中输出；验证 `cargo test` 20 个测试全部通过，`npm.cmd run tauri:build` 通过。
- 2026-05-26：新增可选按时间升序整理；验证 `cargo test` 23 个测试全部通过，`npm.cmd run build` 和 `npm.cmd run tauri:build` 通过。
- 2026-05-29：新增默认启用的增量整理和缓存复用统计；验证 `cargo test` 26 个测试全部通过，`npm.cmd run build` 通过。
- 2026-05-29：新增 XLSX“图片路径”列；验证 `cargo test` 26 个测试全部通过。
- 2026-05-29：将 XLSX“图片路径”调整到最后一列；验证 `cargo test` 26 个测试全部通过，`npm.cmd run tauri:build` 通过。
- 2026-06-03：新增后端多线程图片处理；验证 `cargo test` 26 个测试全部通过。
- 2026-06-03：调整图片文件夹输出策略，单图也会创建并填写 `imageN/` 文件夹，重复图继续合并到代表图文件夹；新增去重开启时唯一图的覆盖测试；验证 `cargo test` 27 个测试全部通过，`npm.cmd run build` 和 `npm.cmd run tauri:build` 通过。
