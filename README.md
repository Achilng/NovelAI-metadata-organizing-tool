# NovelAI 元数据整理工具（已并入智能表格）

> **本工具已停止独立开发，全部功能已并入 [Smart-Spreadsheet（智能表格）](https://github.com/Achilng/Smart-Spreadsheet)。**

自智能表格 v0.3.0 起：

- 文件夹 / 压缩包（zip、7z、rar）的 NovelAI PNG 元数据提取改为**直接导入资料库**，不再经由 xlsx 中转；已入库图片自动跳过，支持增量追加。
- xlsx 输出转为可选导出（带缩略图全新生成）；图片输出包转为按筛选/选中结果导出（复制或硬链接）。
- 导入期去重改为库内查重视图（按正向提示词 / 画师串分组，删除时每组至少保留一行）。
- 智绘姬 JSON 直接从资料库导出；JSON 去重工具已原样移植。

本仓库保留作历史归档，代码仅供参考。请前往
[Smart-Spreadsheet Releases](https://github.com/Achilng/Smart-Spreadsheet/releases)
获取最新版本。
