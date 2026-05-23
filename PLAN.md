# NovelAI PNG 元数据提取器 - 开发计划

## 1. 目标

构建一个面向普通用户的桌面应用。用户选择一个包含 NovelAI 生成 PNG 图片的文件夹或压缩包，选择一个输出 `.xlsx` 路径，应用生成一个 Excel 工作簿，每张图片对应一行。

工作簿列如下：

1. 图片缩略图
2. 正向提示词
3. 负向提示词
4. 画师标签

实现将使用纯 Tauri/Rust 后端，不使用 Python sidecar。

## 2. 已确认需求

- 输入支持：
  - 文件夹
  - `.zip`
  - `.rar`
  - `.7z`
- 只处理 `.png` 图片。
- 输出为用户指定的 `.xlsx` 文件。
- Excel 第一列嵌入图片缩略图。
- 从 NovelAI PNG 元数据中提取正向提示词。
- 从 NovelAI PNG 元数据中提取负向提示词。
- 画师标签只从正向提示词中提取。
- 画师标签提取规则：
  - 将正向提示词拆分为提示词片段。
  - 保留包含 `artist` 的片段，例如 `artist:maidcode1023`、`0.5::artist:xxx::` 和 `-3::artist collaboration::`。
- 应用应通过图形界面对非技术用户可用。

## 3. 约束

- 不升级系统 Node.js 版本。
- 不向 C 盘写入不必要的缓存或临时数据。
- 临时解压和处理文件必须放在 `D:\Agent\Agent_temp` 下。
- 未经用户确认，不得删除超过两个超出已批准任务范围的文件。
- 绝不直接删除 `D:\` 下的顶层文件夹。
- 绝不清空回收站。

## 4. 拟定技术栈

### 桌面外壳

- Tauri v2
- 前端：
  - Vite
  - TypeScript
  - 纯 CSS 或轻量组件方案
- Tauri 插件：
  - `@tauri-apps/plugin-dialog`，用于选择输入和输出路径
  - Tauri command invocation，用于运行提取流程

### Rust 后端

需要评估和使用的 Rust crate：

- PNG 元数据：
  - `png`，用于读取 PNG chunk
  - 或者一个小型自定义 PNG chunk 读取器，用于 `tEXt`、`zTXt` 和 `iTXt`
- JSON：
  - `serde`
  - `serde_json`
- 压缩包：
  - `zip`，用于 `.zip`
  - `sevenz-rust` 或同类方案，用于 `.7z`
  - `unrar`/`unrar_sys`，或基于外部 7z 兼容方案处理 `.rar`
- Excel：
  - 如果图片插入功能足够，优先使用 `rust_xlsxwriter`
  - 否则评估 `umya-spreadsheet` 的图片嵌入支持
- 图片缩略图：
  - `image`，用于将 PNG 图片缩放为临时缩略图文件
- 错误处理：
  - `anyhow`
  - `thiserror`
- 路径和临时文件管理：
  - `tempfile`，配置为使用 `D:\Agent\Agent_temp` 或其下的子目录

## 5. 架构

```text
前端
  - 输入路径选择器
  - 输出 xlsx 选择器
  - 开始按钮
  - 进度/日志区域
  - 完成/错误状态

Tauri command 层
  - 校验路径
  - 调用提取服务
  - 发送进度事件

Rust 提取服务
  - 分类输入路径
  - 从文件夹或已解压压缩包中收集 PNG 文件
  - 读取 PNG 元数据
  - 提取正向提示词
  - 提取负向提示词
  - 提取画师标签
  - 创建缩略图
  - 生成 XLSX
  - 清理临时文件
```

## 6. UI 计划

第一个界面应该直接是实际工具，而不是落地页。

主要控件：

- 输入选择器：
  - 按钮：选择文件夹
  - 按钮：选择压缩包
  - 只读路径显示
- 输出选择器：
  - 按钮：选择 `.xlsx` 保存路径
  - 只读路径显示
- 开始按钮：
  - 在输入和输出都有效之前禁用
- 处理状态：
  - 进度条
  - 当前文件名
  - 计数：
    - PNG 文件总数
    - 已处理文件数
    - 失败文件数
- 结果状态：
  - 成功消息，包含输出路径
  - 无法解析文件的错误列表

视觉风格：

- 安静的桌面工具布局。
- 紧凑、清晰的控件。
- 无营销式 hero 区域。
- 无装饰性渐变/光球背景。
- 确保所有文本都能适配较小的 Windows 笔记本屏幕。

## 7. 元数据提取细节

NovelAI PNG 通常会在 PNG 文本 chunk 中存储文本元数据：

- `Description`
- `Comment`
- `Software`
- `Source`
- `Generation time`

正向提示词提取顺序：

1. `Description`
2. `Comment.prompt`
3. `Comment.v4_prompt.caption.base_caption` 或类似的 v4 结构

负向提示词提取顺序：

1. `Comment.uc`
2. `Comment.v4_negative_prompt.caption.base_caption` 或类似的 v4 结构
3. 如果未找到，则为空字符串

PNG chunk 处理：

- `tEXt`
  - Latin-1 keyword
  - 文本值按 UTF-8 解码，并带有回退处理
- `zTXt`
  - zlib 压缩文本
- `iTXt`
  - UTF-8 国际化文本
  - 支持压缩和未压缩形式

如果元数据格式异常：

- 继续处理其他文件。
- 在 UI 错误列表中记录该文件。
- 如果图片仍然被包含在输出中，则该行的提示词字段为空。

## 8. 画师标签提取

初始规则：

1. 按逗号和换行符拆分正向提示词。
2. 对每个片段裁剪首尾空白。
3. 保留小写文本中包含 `artist` 的片段。
4. 对完全相同的匹配项去重，同时保留原始顺序。
5. 在 Excel 单元格中用换行符连接匹配项。

示例：

```text
0.5::artist:maidcode1023 ::
artist:sune (mugendai)
-3::artist collaboration::
```

## 9. 压缩包处理

### 文件夹输入

- 递归扫描 `.png`。
- 保留相对路径用于显示和错误消息。

### 压缩包输入

- 支持的扩展名：
  - `.zip`
  - `.rar`
  - `.7z`
- 解压到：
  - `D:\Agent\Agent_temp\novelai_metadata_extractor\<run_id>\`
- 递归扫描解压后的文件中的 `.png`。
- 处理完成后清理 run 目录，除非启用了调试模式。

实现偏好：

1. 如果纯 Rust crate 在 Windows 和目标压缩格式上足够可靠，则使用纯 Rust crate。
2. 如果 `.rar` 的纯 Rust 支持不可靠，则评估捆绑 7z 二进制文件作为受控依赖，并在实现前记录取舍。

开放风险：

- 真正纯 Rust 的 `.rar` 解压能力可能弱于 `.zip`/`.7z`。
- 这应在 UI 打磨前，用示例 `.rar` 文件尽早验证。

## 10. XLSX 生成

工作簿布局：

| 列 | 表头 | 内容 |
|---|---|---|
| A | 图片 | 嵌入缩略图 |
| B | 正向提示词 | Positive prompt |
| C | 负向提示词 | Negative prompt |
| D | 画师串 | 提取出的画师片段 |

格式：

- 冻结表头行。
- 表头行加粗。
- 设置列宽：
  - 图片列：固定缩略图宽度
  - 提示词列：较宽的文本列
  - 画师列：中等宽度文本列
- 对提示词和画师单元格启用自动换行。
- 设置行高以适配缩略图。
- 使用最大缩略图尺寸，例如 `160x160`，同时保持宽高比。

临时缩略图：

- 在每次运行的临时目录下生成。
- 工作簿写入后删除。

## 11. 后端命令设计

主 Tauri 命令：

```rust
#[tauri::command]
async fn extract_to_xlsx(input_path: String, output_path: String) -> Result<RunSummary, String>
```

进度事件：

```text
extract:start
extract:scan_complete
extract:file_progress
extract:file_warning
extract:complete
extract:error
```

Summary 结构：

```rust
struct RunSummary {
    total_png: usize,
    processed: usize,
    failed: usize,
    output_path: String,
    warnings: Vec<FileWarning>,
}
```

## 12. 实施步骤

### 第 1 步 - 项目检查

- 检查是否已经存在 Tauri 项目。
- 检查已安装的 Node、npm/pnpm、Rust、Cargo 和 Tauri CLI 是否可用。
- 不升级 Node。如果当前 Node 版本不兼容，则停止并报告。

### 第 2 步 - 搭建应用

- 如果不存在，则创建 Tauri 应用结构。
- 将生成的文件保留在项目目录内。
- 配置开发和构建用的 package scripts。

### 第 3 步 - 后端核心

- 实现 PNG 文本 chunk 读取器。
- 实现 NovelAI 元数据解析器。
- 实现正向提示词提取。
- 实现负向提示词提取。
- 实现画师提取。
- 使用小型元数据 fixture 添加单元测试。

### 第 4 步 - 压缩包和文件夹输入

- 实现递归文件夹扫描。
- 实现 `.zip` 解压。
- 实现 `.7z` 解压。
- 调研并实现 `.rar` 解压。
- 确保临时文件位于 `D:\Agent\Agent_temp` 下。
- 添加文件夹扫描和压缩包解压测试。

### 第 5 步 - XLSX 写入器

- 评估所选 crate 的图片嵌入支持。
- 实现工作簿生成。
- 添加缩略图创建。
- 添加自动换行单元格和列/行尺寸设置。
- 使用示例 NovelAI PNG 测试。

### 第 6 步 - Tauri 命令集成

- 暴露 `extract_to_xlsx`。
- 添加进度事件。
- 校验输入/输出路径。
- 返回用户可读的错误。

### 第 7 步 - 前端

- 构建主工具 UI。
- 通过 dialog 插件添加文件夹/压缩包选择。
- 通过 dialog 插件添加保存路径选择。
- 调用后端命令。
- 渲染进度和警告。
- 处理期间禁用重复运行。

### 第 8 步 - 端到端测试

- 使用以下场景测试：
  - 单个 PNG 文件夹
  - 嵌套 PNG 文件夹
  - `.zip`
  - `.7z`
  - `.rar`
  - 空文件夹
  - 非 NovelAI PNG
  - 损坏的 PNG
- 验证：
  - Excel 能正确打开。
  - 缩略图能显示。
  - 正向提示词与元数据一致。
  - 负向提示词与元数据一致。
  - 画师标签只从正向提示词中提取。
  - 临时文件已清理。

### 第 9 步 - 打包

- 构建 Windows 桌面安装包。
- 确认应用无需开发工具即可运行。
- 记录任何外部运行时需求。

## 13. 验收标准

- 用户可以从 GUI 选择文件夹或 `.zip`/`.rar`/`.7z` 压缩包。
- 用户可以选择 `.xlsx` 输出路径。
- 应用生成包含图片缩略图和提取出的提示词数据的 Excel 文件。
- 应用在处理多个 PNG 文件时，不会因单个异常文件而崩溃。
- 应用能清楚报告失败文件。
- 应用不会向 C 盘写入临时文件。
- 应用不要求用户使用命令行。

## 14. 已知风险

- `.rar` 支持可能需要特殊处理。
- Excel 图片插入支持必须尽早验证。
- NovelAI 元数据结构可能因模型版本而异。
- 超大图片批次可能需要进度取消和内存保护机制。

## 15. 未来增强

- 添加取消按钮。
- 添加拖放输入。
- 添加在 Excel 中包含源文件路径的选项。
- 添加导出所有元数据字段的选项。
- 添加导出前预览表格。