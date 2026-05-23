import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import "./styles.css";

type RunSummary = {
  total_png: number;
  processed: number;
  failed: number;
  output_path: string;
  warnings: FileWarning[];
};

type FileWarning = {
  path: string;
  message: string;
};

type ProgressPayload = {
  total_png?: number;
  processed?: number;
  failed?: number;
  current_file?: string;
  message?: string;
};

const state = {
  inputPath: "",
  outputPath: "",
  isRunning: false,
  processed: 0,
  failed: 0,
  total: 0,
  currentFile: "",
  status: "请选择输入和输出路径。",
  warnings: [] as FileWarning[]
};

document.querySelector<HTMLDivElement>("#app")!.innerHTML = `
  <section class="shell">
    <header class="topbar">
      <div>
        <h1>NovelAI 元数据整理工具</h1>
        <p>从 NovelAI PNG 图片中提取提示词并生成 Excel 工作簿。</p>
      </div>
      <span class="version">v0.1.0</span>
    </header>

    <section class="panel">
      <div class="section-title">输入</div>
      <div class="row">
        <button id="choose-folder" type="button">选择文件夹</button>
        <button id="choose-archive" type="button">选择压缩包</button>
        <output id="input-path" class="path">未选择</output>
      </div>
    </section>

    <section class="panel">
      <div class="section-title">输出</div>
      <div class="row">
        <button id="choose-output" type="button">选择 .xlsx 保存路径</button>
        <output id="output-path" class="path">未选择</output>
      </div>
    </section>

    <section class="panel status-panel">
      <div class="status-head">
        <div>
          <div class="section-title">处理状态</div>
          <p id="status-message">请选择输入和输出路径。</p>
        </div>
        <button id="start" class="primary" type="button" disabled>开始整理</button>
      </div>

      <progress id="progress" max="100" value="0"></progress>
      <div class="stats">
        <span>总数 <strong id="total">0</strong></span>
        <span>已处理 <strong id="processed">0</strong></span>
        <span>失败 <strong id="failed">0</strong></span>
      </div>
      <div class="current-file">
        <span>当前文件</span>
        <output id="current-file">-</output>
      </div>
    </section>

    <section class="panel">
      <div class="section-title">警告</div>
      <ul id="warnings" class="warnings">
        <li class="empty">暂无警告</li>
      </ul>
    </section>
  </section>
`;

const chooseFolderButton = document.querySelector<HTMLButtonElement>("#choose-folder")!;
const chooseArchiveButton = document.querySelector<HTMLButtonElement>("#choose-archive")!;
const chooseOutputButton = document.querySelector<HTMLButtonElement>("#choose-output")!;
const startButton = document.querySelector<HTMLButtonElement>("#start")!;
const inputPathOutput = document.querySelector<HTMLOutputElement>("#input-path")!;
const outputPathOutput = document.querySelector<HTMLOutputElement>("#output-path")!;
const statusMessage = document.querySelector<HTMLParagraphElement>("#status-message")!;
const progress = document.querySelector<HTMLProgressElement>("#progress")!;
const total = document.querySelector<HTMLElement>("#total")!;
const processed = document.querySelector<HTMLElement>("#processed")!;
const failed = document.querySelector<HTMLElement>("#failed")!;
const currentFile = document.querySelector<HTMLOutputElement>("#current-file")!;
const warnings = document.querySelector<HTMLUListElement>("#warnings")!;

function render() {
  inputPathOutput.textContent = state.inputPath || "未选择";
  outputPathOutput.textContent = state.outputPath || "未选择";
  statusMessage.textContent = state.status;
  total.textContent = String(state.total);
  processed.textContent = String(state.processed);
  failed.textContent = String(state.failed);
  currentFile.textContent = state.currentFile || "-";

  const percent = state.total === 0 ? 0 : Math.round((state.processed / state.total) * 100);
  progress.value = percent;

  const canStart = Boolean(state.inputPath && state.outputPath && !state.isRunning);
  startButton.disabled = !canStart;
  chooseFolderButton.disabled = state.isRunning;
  chooseArchiveButton.disabled = state.isRunning;
  chooseOutputButton.disabled = state.isRunning;

  warnings.innerHTML = "";
  if (state.warnings.length === 0) {
    const item = document.createElement("li");
    item.className = "empty";
    item.textContent = "暂无警告";
    warnings.append(item);
  } else {
    for (const warning of state.warnings) {
      const item = document.createElement("li");
      item.innerHTML = `<strong></strong><span></span>`;
      item.querySelector("strong")!.textContent = warning.path;
      item.querySelector("span")!.textContent = warning.message;
      warnings.append(item);
    }
  }
}

function normalizeSelectedPath(value: string | string[] | null): string {
  if (Array.isArray(value)) {
    return value[0] ?? "";
  }
  return value ?? "";
}

chooseFolderButton.addEventListener("click", async () => {
  const selected = await open({
    directory: true,
    multiple: false
  });
  state.inputPath = normalizeSelectedPath(selected);
  state.status = state.inputPath ? "已选择输入文件夹。" : state.status;
  render();
});

chooseArchiveButton.addEventListener("click", async () => {
  const selected = await open({
    directory: false,
    multiple: false,
    filters: [{ name: "压缩包", extensions: ["zip", "rar", "7z"] }]
  });
  state.inputPath = normalizeSelectedPath(selected);
  state.status = state.inputPath ? "已选择输入压缩包。" : state.status;
  render();
});

chooseOutputButton.addEventListener("click", async () => {
  const selected = await save({
    defaultPath: "novelai_metadata.xlsx",
    filters: [{ name: "Excel 工作簿", extensions: ["xlsx"] }]
  });
  state.outputPath = normalizeSelectedPath(selected);
  state.status = state.outputPath ? "已选择输出路径。" : state.status;
  render();
});

startButton.addEventListener("click", async () => {
  if (!state.inputPath || !state.outputPath || state.isRunning) {
    return;
  }

  state.isRunning = true;
  state.total = 0;
  state.processed = 0;
  state.failed = 0;
  state.currentFile = "";
  state.warnings = [];
  state.status = "正在处理...";
  render();

  try {
    const summary = await invoke<RunSummary>("extract_to_xlsx", {
      inputPath: state.inputPath,
      outputPath: state.outputPath
    });
    state.total = summary.total_png;
    state.processed = summary.processed;
    state.failed = summary.failed;
    state.warnings = summary.warnings;
    state.status = `完成：已生成 ${summary.output_path}`;
  } catch (error) {
    state.status = error instanceof Error ? error.message : String(error);
  } finally {
    state.isRunning = false;
    state.currentFile = "";
    render();
  }
});

await listen<ProgressPayload>("extract:start", (event) => {
  state.total = event.payload.total_png ?? 0;
  state.processed = 0;
  state.failed = 0;
  state.status = event.payload.message ?? "开始处理...";
  render();
});

await listen<ProgressPayload>("extract:scan_complete", (event) => {
  state.total = event.payload.total_png ?? state.total;
  state.status = event.payload.message ?? "扫描完成。";
  render();
});

await listen<ProgressPayload>("extract:file_progress", (event) => {
  state.processed = event.payload.processed ?? state.processed;
  state.failed = event.payload.failed ?? state.failed;
  state.currentFile = event.payload.current_file ?? "";
  state.status = event.payload.message ?? "正在处理...";
  render();
});

await listen<FileWarning>("extract:file_warning", (event) => {
  state.warnings.push(event.payload);
  render();
});

render();

