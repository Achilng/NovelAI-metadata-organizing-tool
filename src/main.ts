import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask, open, save } from "@tauri-apps/plugin-dialog";
import "./styles.css";

type RunSummary = {
  total_png: number;
  processed: number;
  failed: number;
  skipped_duplicates: number;
  cache_hits: number;
  processed_new: number;
  output_path: string;
  warnings: FileWarning[];
};

type FileWarning = {
  path: string;
  message: string;
};

type ImageOutputMode = "copy" | "hardlink" | "none";

type CacheClearSummary = {
  existed: boolean;
  removed_files: number;
  freed_bytes: number;
};

type ProgressPayload = {
  total_png?: number;
  processed?: number;
  failed?: number;
  skipped_duplicates?: number;
  cache_hits?: number;
  processed_new?: number;
  current_file?: string;
  message?: string;
};

type ConversionPreviewItem = {
  fixed_prompt: string;
  negative_prompt: string;
};

type XlsxInspection = {
  record_count: number;
  preview: ConversionPreviewItem[];
};

type ConversionProgress = {
  total: number;
  processed: number;
  message: string;
};

type ConversionSummary = {
  exported: number;
  output_path: string;
};

type JsonDedupePreviewItem = {
  preset_key: string;
  fixed_prompt: string;
  negative_prompt: string;
};

type JsonDedupeInspection = {
  original_count: number;
  duplicate_count: number;
  unique_count: number;
  preview: JsonDedupePreviewItem[];
};

type JsonDedupeProgress = {
  total: number;
  processed: number;
  duplicate_count: number;
  message: string;
};

type JsonDedupeSummary = {
  original_count: number;
  duplicate_count: number;
  unique_count: number;
  output_path: string;
};

type ActiveTab = "organizer" | "converter" | "jsonDedupe";

const organizerState = {
  inputPath: "",
  outputPath: "",
  isRunning: false,
  dedupePositivePrompt: false,
  dedupeArtistTags: false,
  sortByTime: false,
  incremental: true,
  imageOutputMode: "copy" as ImageOutputMode,
  isClearingCache: false,
  processed: 0,
  failed: 0,
  skippedDuplicates: 0,
  cacheHits: 0,
  processedNew: 0,
  total: 0,
  currentFile: "",
  status: "请选择输入和输出路径。",
  warnings: [] as FileWarning[]
};

const converterState = {
  inputPath: "",
  outputPath: "",
  isInspecting: false,
  isRunning: false,
  inspection: null as XlsxInspection | null,
  processed: 0,
  status: "请选择本工具生成的 XLSX 文件。",
  completedOutputPath: ""
};

const jsonDedupeState = {
  inputPath: "",
  outputPath: "",
  isInspecting: false,
  isRunning: false,
  inspection: null as JsonDedupeInspection | null,
  processed: 0,
  duplicateCount: 0,
  status: "请选择智绘姬 JSON 文件。",
  completedOutputPath: ""
};

let activeTab: ActiveTab = "organizer";
let inspectionSequence = 0;
let jsonDedupeInspectionSequence = 0;

document.querySelector<HTMLDivElement>("#app")!.innerHTML = `
  <section class="shell">
    <header class="topbar">
      <div>
        <h1>NovelAI 元数据整理工具</h1>
        <p>整理 NovelAI 图片元数据、转换智绘姬 JSON，或清理其中的重复提示词。</p>
      </div>
      <span class="version">v0.1.0</span>
    </header>

    <nav class="tabs" aria-label="工具页面">
      <button id="organizer-tab" class="tab active" type="button" aria-selected="true">图片整理</button>
      <button id="converter-tab" class="tab" type="button" aria-selected="false">XLSX转智绘姬JSON格式</button>
      <button id="json-dedupe-tab" class="tab" type="button" aria-selected="false">JSON去重</button>
    </nav>

    <div id="organizer-page" class="tab-page">
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
          <button id="choose-output" type="button">选择输出路径</button>
          <button id="clear-cache" type="button" disabled>清理缓存</button>
          <output id="output-path" class="path">未选择</output>
        </div>
      </section>

      <section class="panel">
        <div class="section-title">整理选项</div>
        <div class="options-grid">
          <label class="toggle-option">
            <input id="dedupe-positive-prompt" type="checkbox" />
            <span>正面提示词去重</span>
          </label>
          <label class="toggle-option">
            <input id="dedupe-artist-tags" type="checkbox" />
            <span>画师串去重</span>
          </label>
          <label class="toggle-option">
            <input id="sort-by-time" type="checkbox" />
            <span>按时间升序整理</span>
          </label>
          <label class="toggle-option">
            <input id="incremental" type="checkbox" />
            <span>增量整理</span>
          </label>
          <label class="toggle-option select-option">
            <span>图片输出</span>
            <select id="image-output-mode">
              <option value="copy">复制原图</option>
              <option value="hardlink">硬链接（同分区不占空间）</option>
              <option value="none">不输出图片文件夹</option>
            </select>
          </label>
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
          <span>去重跳过 <strong id="skipped-duplicates">0</strong></span>
          <span>缓存复用 <strong id="cache-hits">0</strong></span>
          <span>新处理 <strong id="processed-new">0</strong></span>
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
    </div>

    <div id="converter-page" class="tab-page" hidden>
      <section class="panel">
        <div class="section-title">XLSX 输入</div>
        <div class="converter-row">
          <button id="choose-converter-input" type="button">选择 XLSX</button>
          <output id="converter-input-path" class="path">未选择</output>
        </div>
      </section>

      <section class="panel">
        <div class="section-title">JSON 输出</div>
        <div class="converter-row">
          <button id="choose-converter-output" type="button">选择 JSON 路径</button>
          <output id="converter-output-path" class="path">未选择</output>
        </div>
      </section>

      <section class="panel mapping-panel">
        <div class="section-title">转换规则</div>
        <div class="mapping-grid">
          <span><strong>正向提示词</strong> → fixedPrompt</span>
          <span><strong>负向提示词</strong> → negativePrompt</span>
          <span><strong>fixedPrompt_end</strong> → 空字符串</span>
          <span><strong>images</strong> → 空对象</span>
        </div>
      </section>

      <section class="panel">
        <div class="preview-head">
          <div>
            <div class="section-title">数据检查与预览</div>
            <p id="inspection-status">选择 XLSX 后会检查表头并预览前 3 条记录。</p>
          </div>
          <span class="record-badge">有效记录 <strong id="converter-total">0</strong></span>
        </div>
        <div id="converter-preview" class="preview-list">
          <p class="empty-preview">暂无预览</p>
        </div>
      </section>

      <section class="panel status-panel">
        <div class="status-head">
          <div>
            <div class="section-title">转换状态</div>
            <p id="converter-status">请选择本工具生成的 XLSX 文件。</p>
          </div>
          <div class="action-row">
            <button id="open-output-folder" type="button" disabled>打开所在文件夹</button>
            <button id="start-conversion" class="primary" type="button" disabled>开始转换</button>
          </div>
        </div>
        <progress id="converter-progress" max="100" value="0"></progress>
        <div class="stats">
          <span>总记录 <strong id="conversion-total">0</strong></span>
          <span>已导出 <strong id="conversion-processed">0</strong></span>
        </div>
      </section>
    </div>

    <div id="json-dedupe-page" class="tab-page" hidden>
      <section class="panel">
        <div class="section-title">JSON 输入</div>
        <div class="converter-row">
          <button id="choose-json-dedupe-input" type="button">选择 JSON</button>
          <output id="json-dedupe-input-path" class="path">未选择</output>
        </div>
      </section>

      <section class="panel">
        <div class="section-title">JSON 输出</div>
        <div class="converter-row">
          <button id="choose-json-dedupe-output" type="button">选择输出路径</button>
          <output id="json-dedupe-output-path" class="path">未选择</output>
        </div>
      </section>

      <section class="panel mapping-panel">
        <div class="section-title">去重规则</div>
        <div class="mapping-grid">
          <span><strong>比较字段</strong> → fixedPrompt</span>
          <span><strong>首尾空白</strong> → 忽略</span>
          <span><strong>空提示词</strong> → 全部保留</span>
          <span><strong>重复记录</strong> → 保留第一条并连续编号</span>
        </div>
      </section>

      <section class="panel">
        <div class="preview-head">
          <div>
            <div class="section-title">数据检查与预览</div>
            <p id="json-dedupe-inspection-status">选择 JSON 后会检查 presets 并预览前 3 条记录。</p>
          </div>
          <span class="record-badge">预计保留 <strong id="json-dedupe-unique-badge">0</strong></span>
        </div>
        <div id="json-dedupe-preview" class="preview-list">
          <p class="empty-preview">暂无预览</p>
        </div>
      </section>

      <section class="panel status-panel">
        <div class="status-head">
          <div>
            <div class="section-title">去重状态</div>
            <p id="json-dedupe-status">请选择智绘姬 JSON 文件。</p>
          </div>
          <div class="action-row">
            <button id="open-json-dedupe-output-folder" type="button" disabled>打开所在文件夹</button>
            <button id="start-json-dedupe" class="primary" type="button" disabled>开始去重</button>
          </div>
        </div>
        <progress id="json-dedupe-progress" max="100" value="0"></progress>
        <div class="stats">
          <span>原始记录 <strong id="json-dedupe-total">0</strong></span>
          <span>已检查 <strong id="json-dedupe-processed">0</strong></span>
          <span>删除重复 <strong id="json-dedupe-duplicates">0</strong></span>
          <span>最终保留 <strong id="json-dedupe-unique">0</strong></span>
        </div>
      </section>
    </div>
  </section>
`;

const organizerTab = document.querySelector<HTMLButtonElement>("#organizer-tab")!;
const converterTab = document.querySelector<HTMLButtonElement>("#converter-tab")!;
const jsonDedupeTab = document.querySelector<HTMLButtonElement>("#json-dedupe-tab")!;
const organizerPage = document.querySelector<HTMLDivElement>("#organizer-page")!;
const converterPage = document.querySelector<HTMLDivElement>("#converter-page")!;
const jsonDedupePage = document.querySelector<HTMLDivElement>("#json-dedupe-page")!;

const chooseFolderButton = document.querySelector<HTMLButtonElement>("#choose-folder")!;
const chooseArchiveButton = document.querySelector<HTMLButtonElement>("#choose-archive")!;
const chooseOutputButton = document.querySelector<HTMLButtonElement>("#choose-output")!;
const clearCacheButton = document.querySelector<HTMLButtonElement>("#clear-cache")!;
const imageOutputModeSelect = document.querySelector<HTMLSelectElement>("#image-output-mode")!;
const startButton = document.querySelector<HTMLButtonElement>("#start")!;
const dedupePositivePromptCheckbox = document.querySelector<HTMLInputElement>(
  "#dedupe-positive-prompt"
)!;
const dedupeArtistTagsCheckbox = document.querySelector<HTMLInputElement>("#dedupe-artist-tags")!;
const sortByTimeCheckbox = document.querySelector<HTMLInputElement>("#sort-by-time")!;
const incrementalCheckbox = document.querySelector<HTMLInputElement>("#incremental")!;
const inputPathOutput = document.querySelector<HTMLOutputElement>("#input-path")!;
const outputPathOutput = document.querySelector<HTMLOutputElement>("#output-path")!;
const statusMessage = document.querySelector<HTMLParagraphElement>("#status-message")!;
const progress = document.querySelector<HTMLProgressElement>("#progress")!;
const total = document.querySelector<HTMLElement>("#total")!;
const processed = document.querySelector<HTMLElement>("#processed")!;
const failed = document.querySelector<HTMLElement>("#failed")!;
const skippedDuplicates = document.querySelector<HTMLElement>("#skipped-duplicates")!;
const cacheHits = document.querySelector<HTMLElement>("#cache-hits")!;
const processedNew = document.querySelector<HTMLElement>("#processed-new")!;
const currentFile = document.querySelector<HTMLOutputElement>("#current-file")!;
const warnings = document.querySelector<HTMLUListElement>("#warnings")!;

const chooseConverterInputButton = document.querySelector<HTMLButtonElement>(
  "#choose-converter-input"
)!;
const chooseConverterOutputButton = document.querySelector<HTMLButtonElement>(
  "#choose-converter-output"
)!;
const converterInputPath = document.querySelector<HTMLOutputElement>("#converter-input-path")!;
const converterOutputPath = document.querySelector<HTMLOutputElement>("#converter-output-path")!;
const inspectionStatus = document.querySelector<HTMLParagraphElement>("#inspection-status")!;
const converterTotal = document.querySelector<HTMLElement>("#converter-total")!;
const converterPreview = document.querySelector<HTMLDivElement>("#converter-preview")!;
const converterStatus = document.querySelector<HTMLParagraphElement>("#converter-status")!;
const converterProgress = document.querySelector<HTMLProgressElement>("#converter-progress")!;
const conversionTotal = document.querySelector<HTMLElement>("#conversion-total")!;
const conversionProcessed = document.querySelector<HTMLElement>("#conversion-processed")!;
const startConversionButton = document.querySelector<HTMLButtonElement>("#start-conversion")!;
const openOutputFolderButton = document.querySelector<HTMLButtonElement>("#open-output-folder")!;

const chooseJsonDedupeInputButton = document.querySelector<HTMLButtonElement>(
  "#choose-json-dedupe-input"
)!;
const chooseJsonDedupeOutputButton = document.querySelector<HTMLButtonElement>(
  "#choose-json-dedupe-output"
)!;
const jsonDedupeInputPath = document.querySelector<HTMLOutputElement>(
  "#json-dedupe-input-path"
)!;
const jsonDedupeOutputPath = document.querySelector<HTMLOutputElement>(
  "#json-dedupe-output-path"
)!;
const jsonDedupeInspectionStatus = document.querySelector<HTMLParagraphElement>(
  "#json-dedupe-inspection-status"
)!;
const jsonDedupeUniqueBadge = document.querySelector<HTMLElement>("#json-dedupe-unique-badge")!;
const jsonDedupePreview = document.querySelector<HTMLDivElement>("#json-dedupe-preview")!;
const jsonDedupeStatus = document.querySelector<HTMLParagraphElement>("#json-dedupe-status")!;
const jsonDedupeProgress = document.querySelector<HTMLProgressElement>("#json-dedupe-progress")!;
const jsonDedupeTotal = document.querySelector<HTMLElement>("#json-dedupe-total")!;
const jsonDedupeProcessed = document.querySelector<HTMLElement>("#json-dedupe-processed")!;
const jsonDedupeDuplicates = document.querySelector<HTMLElement>("#json-dedupe-duplicates")!;
const jsonDedupeUnique = document.querySelector<HTMLElement>("#json-dedupe-unique")!;
const startJsonDedupeButton = document.querySelector<HTMLButtonElement>("#start-json-dedupe")!;
const openJsonDedupeOutputFolderButton = document.querySelector<HTMLButtonElement>(
  "#open-json-dedupe-output-folder"
)!;

function render() {
  const anyRunning =
    organizerState.isRunning || converterState.isRunning || jsonDedupeState.isRunning;
  organizerPage.hidden = activeTab !== "organizer";
  converterPage.hidden = activeTab !== "converter";
  jsonDedupePage.hidden = activeTab !== "jsonDedupe";
  organizerTab.classList.toggle("active", activeTab === "organizer");
  converterTab.classList.toggle("active", activeTab === "converter");
  jsonDedupeTab.classList.toggle("active", activeTab === "jsonDedupe");
  organizerTab.setAttribute("aria-selected", String(activeTab === "organizer"));
  converterTab.setAttribute("aria-selected", String(activeTab === "converter"));
  jsonDedupeTab.setAttribute("aria-selected", String(activeTab === "jsonDedupe"));
  organizerTab.disabled = anyRunning;
  converterTab.disabled = anyRunning;
  jsonDedupeTab.disabled = anyRunning;

  inputPathOutput.textContent = organizerState.inputPath || "未选择";
  outputPathOutput.textContent = organizerState.outputPath || "未选择";
  statusMessage.textContent = organizerState.status;
  total.textContent = String(organizerState.total);
  processed.textContent = String(organizerState.processed);
  failed.textContent = String(organizerState.failed);
  skippedDuplicates.textContent = String(organizerState.skippedDuplicates);
  cacheHits.textContent = String(organizerState.cacheHits);
  processedNew.textContent = String(organizerState.processedNew);
  currentFile.textContent = organizerState.currentFile || "-";
  progress.value = percentage(organizerState.processed, organizerState.total);

  const canStart = Boolean(
    organizerState.inputPath &&
      organizerState.outputPath &&
      !organizerState.isRunning &&
      !organizerState.isClearingCache
  );
  startButton.disabled = !canStart;
  chooseFolderButton.disabled = organizerState.isRunning;
  chooseArchiveButton.disabled = organizerState.isRunning;
  chooseOutputButton.disabled = organizerState.isRunning;
  clearCacheButton.disabled =
    organizerState.isRunning || organizerState.isClearingCache || !organizerState.outputPath;
  imageOutputModeSelect.value = organizerState.imageOutputMode;
  imageOutputModeSelect.disabled = organizerState.isRunning;
  dedupePositivePromptCheckbox.checked = organizerState.dedupePositivePrompt;
  dedupePositivePromptCheckbox.disabled = organizerState.isRunning;
  dedupeArtistTagsCheckbox.checked = organizerState.dedupeArtistTags;
  dedupeArtistTagsCheckbox.disabled = organizerState.isRunning;
  sortByTimeCheckbox.checked = organizerState.sortByTime;
  sortByTimeCheckbox.disabled = organizerState.isRunning;
  incrementalCheckbox.checked = organizerState.incremental;
  incrementalCheckbox.disabled = organizerState.isRunning;
  renderWarnings();

  converterInputPath.textContent = converterState.inputPath || "未选择";
  converterOutputPath.textContent = converterState.outputPath || "未选择";
  converterStatus.textContent = converterState.status;
  const recordCount = converterState.inspection?.record_count ?? 0;
  converterTotal.textContent = String(recordCount);
  conversionTotal.textContent = String(recordCount);
  conversionProcessed.textContent = String(converterState.processed);
  converterProgress.value = percentage(converterState.processed, recordCount);
  inspectionStatus.textContent = converterState.isInspecting
    ? "正在检查 XLSX..."
    : converterState.inspection
      ? `检查完成：找到 ${recordCount} 条有效记录。`
      : "选择 XLSX 后会检查表头并预览前 3 条记录。";
  chooseConverterInputButton.disabled = converterState.isInspecting || converterState.isRunning;
  chooseConverterOutputButton.disabled = converterState.isInspecting || converterState.isRunning;
  startConversionButton.disabled = !Boolean(
    converterState.inspection &&
      converterState.inputPath &&
      converterState.outputPath &&
      !converterState.isInspecting &&
      !converterState.isRunning
  );
  openOutputFolderButton.disabled = !Boolean(
    converterState.completedOutputPath && !converterState.isRunning
  );
  renderPreview();

  jsonDedupeInputPath.textContent = jsonDedupeState.inputPath || "未选择";
  jsonDedupeOutputPath.textContent = jsonDedupeState.outputPath || "未选择";
  jsonDedupeStatus.textContent = jsonDedupeState.status;
  const jsonDedupeOriginalCount = jsonDedupeState.inspection?.original_count ?? 0;
  const jsonDedupeUniqueCount = jsonDedupeState.inspection?.unique_count ?? 0;
  jsonDedupeTotal.textContent = String(jsonDedupeOriginalCount);
  jsonDedupeProcessed.textContent = String(jsonDedupeState.processed);
  jsonDedupeDuplicates.textContent = String(jsonDedupeState.duplicateCount);
  jsonDedupeUnique.textContent = String(jsonDedupeUniqueCount);
  jsonDedupeUniqueBadge.textContent = String(jsonDedupeUniqueCount);
  jsonDedupeProgress.value = percentage(jsonDedupeState.processed, jsonDedupeOriginalCount);
  jsonDedupeInspectionStatus.textContent = jsonDedupeState.isInspecting
    ? "正在检查 JSON..."
    : jsonDedupeState.inspection
      ? `检查完成：共 ${jsonDedupeOriginalCount} 条，发现 ${jsonDedupeState.inspection.duplicate_count} 条重复。`
      : "选择 JSON 后会检查 presets 并预览前 3 条记录。";
  chooseJsonDedupeInputButton.disabled =
    jsonDedupeState.isInspecting || jsonDedupeState.isRunning;
  chooseJsonDedupeOutputButton.disabled =
    jsonDedupeState.isInspecting || jsonDedupeState.isRunning;
  startJsonDedupeButton.disabled = !Boolean(
    jsonDedupeState.inspection &&
      jsonDedupeState.inputPath &&
      jsonDedupeState.outputPath &&
      !jsonDedupeState.isInspecting &&
      !jsonDedupeState.isRunning
  );
  openJsonDedupeOutputFolderButton.disabled = !Boolean(
    jsonDedupeState.completedOutputPath && !jsonDedupeState.isRunning
  );
  renderJsonDedupePreview();
}

function renderWarnings() {
  warnings.innerHTML = "";
  if (organizerState.warnings.length === 0) {
    const item = document.createElement("li");
    item.className = "empty";
    item.textContent = "暂无警告";
    warnings.append(item);
    return;
  }

  for (const warning of organizerState.warnings) {
    const item = document.createElement("li");
    item.innerHTML = `<strong></strong><span></span>`;
    item.querySelector("strong")!.textContent = warning.path;
    item.querySelector("span")!.textContent = warning.message;
    warnings.append(item);
  }
}

function renderPreview() {
  converterPreview.innerHTML = "";
  const preview = converterState.inspection?.preview ?? [];
  if (preview.length === 0) {
    const empty = document.createElement("p");
    empty.className = "empty-preview";
    empty.textContent = converterState.inspection ? "XLSX 中没有有效记录" : "暂无预览";
    converterPreview.append(empty);
    return;
  }

  preview.forEach((item, index) => {
    const card = document.createElement("article");
    card.className = "preview-card";
    card.innerHTML = `
      <div class="preview-index"></div>
      <div class="preview-field"><strong>fixedPrompt</strong><pre></pre></div>
      <div class="preview-field"><strong>negativePrompt</strong><pre></pre></div>
    `;
    card.querySelector(".preview-index")!.textContent = `记录 ${index + 1}`;
    const values = card.querySelectorAll("pre");
    values[0].textContent = item.fixed_prompt || "（空）";
    values[1].textContent = item.negative_prompt || "（空）";
    converterPreview.append(card);
  });
}

function renderJsonDedupePreview() {
  jsonDedupePreview.innerHTML = "";
  const preview = jsonDedupeState.inspection?.preview ?? [];
  if (preview.length === 0) {
    const empty = document.createElement("p");
    empty.className = "empty-preview";
    empty.textContent = jsonDedupeState.inspection ? "presets 中没有记录" : "暂无预览";
    jsonDedupePreview.append(empty);
    return;
  }

  preview.forEach((item) => {
    const card = document.createElement("article");
    card.className = "preview-card";
    card.innerHTML = `
      <div class="preview-index"></div>
      <div class="preview-field"><strong>fixedPrompt</strong><pre></pre></div>
      <div class="preview-field"><strong>negativePrompt</strong><pre></pre></div>
    `;
    card.querySelector(".preview-index")!.textContent = `原始键 ${item.preset_key}`;
    const values = card.querySelectorAll("pre");
    values[0].textContent = item.fixed_prompt || "（空）";
    values[1].textContent = item.negative_prompt || "（空）";
    jsonDedupePreview.append(card);
  });
}

function percentage(processedCount: number, totalCount: number): number {
  return totalCount === 0 ? 0 : Math.round((processedCount / totalCount) * 100);
}

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024 * 1024) {
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }
  if (bytes >= 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
  if (bytes >= 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${bytes} B`;
}

function normalizeSelectedPath(value: string | string[] | null): string {
  if (Array.isArray(value)) {
    return value[0] ?? "";
  }
  return value ?? "";
}

function suggestedJsonPath(xlsxPath: string): string {
  return /\.xlsx$/i.test(xlsxPath) ? xlsxPath.replace(/\.xlsx$/i, ".json") : `${xlsxPath}.json`;
}

function suggestedDedupedJsonPath(jsonPath: string): string {
  return /\.json$/i.test(jsonPath)
    ? jsonPath.replace(/\.json$/i, "_deduped.json")
    : `${jsonPath}_deduped.json`;
}

function selectTab(tab: ActiveTab) {
  if (organizerState.isRunning || converterState.isRunning || jsonDedupeState.isRunning) {
    return;
  }
  activeTab = tab;
  render();
}

organizerTab.addEventListener("click", () => selectTab("organizer"));
converterTab.addEventListener("click", () => selectTab("converter"));
jsonDedupeTab.addEventListener("click", () => selectTab("jsonDedupe"));

chooseFolderButton.addEventListener("click", async () => {
  const selected = await open({ directory: true, multiple: false });
  organizerState.inputPath = normalizeSelectedPath(selected);
  organizerState.status = organizerState.inputPath ? "已选择输入文件夹。" : organizerState.status;
  render();
});

chooseArchiveButton.addEventListener("click", async () => {
  const selected = await open({
    directory: false,
    multiple: false,
    filters: [{ name: "压缩包", extensions: ["zip", "rar", "7z"] }]
  });
  organizerState.inputPath = normalizeSelectedPath(selected);
  organizerState.status = organizerState.inputPath ? "已选择输入压缩包。" : organizerState.status;
  render();
});

chooseOutputButton.addEventListener("click", async () => {
  const selected = await save({
    defaultPath: "novelai_metadata.xlsx",
    filters: [{ name: "Excel 工作簿", extensions: ["xlsx"] }]
  });
  organizerState.outputPath = normalizeSelectedPath(selected);
  organizerState.status = organizerState.outputPath ? "已选择输出路径。" : organizerState.status;
  render();
});

dedupePositivePromptCheckbox.addEventListener("change", () => {
  organizerState.dedupePositivePrompt = dedupePositivePromptCheckbox.checked;
  render();
});

dedupeArtistTagsCheckbox.addEventListener("change", () => {
  organizerState.dedupeArtistTags = dedupeArtistTagsCheckbox.checked;
  render();
});

sortByTimeCheckbox.addEventListener("change", () => {
  organizerState.sortByTime = sortByTimeCheckbox.checked;
  render();
});

incrementalCheckbox.addEventListener("change", () => {
  organizerState.incremental = incrementalCheckbox.checked;
  render();
});

imageOutputModeSelect.addEventListener("change", () => {
  organizerState.imageOutputMode = imageOutputModeSelect.value as ImageOutputMode;
  render();
});

clearCacheButton.addEventListener("click", async () => {
  if (!organizerState.outputPath || organizerState.isRunning || organizerState.isClearingCache) {
    return;
  }

  const confirmed = await ask(
    "将删除输出路径同级的 .novelai_metadata_cache 缓存目录，下次整理需要重新解析全部图片。确定清理吗？",
    { title: "清理缓存", kind: "warning" }
  );
  if (!confirmed) {
    return;
  }

  organizerState.isClearingCache = true;
  organizerState.status = "正在清理缓存...";
  render();

  try {
    const summary = await invoke<CacheClearSummary>("clear_metadata_cache", {
      outputPath: organizerState.outputPath
    });
    organizerState.status = summary.existed
      ? `缓存清理完成：删除 ${summary.removed_files} 个文件，释放 ${formatBytes(summary.freed_bytes)}。`
      : "未找到缓存目录，无需清理。";
  } catch (error) {
    organizerState.status = error instanceof Error ? error.message : String(error);
  } finally {
    organizerState.isClearingCache = false;
    render();
  }
});

startButton.addEventListener("click", async () => {
  if (!organizerState.inputPath || !organizerState.outputPath || organizerState.isRunning) {
    return;
  }

  organizerState.isRunning = true;
  organizerState.total = 0;
  organizerState.processed = 0;
  organizerState.failed = 0;
  organizerState.skippedDuplicates = 0;
  organizerState.cacheHits = 0;
  organizerState.processedNew = 0;
  organizerState.currentFile = "";
  organizerState.warnings = [];
  organizerState.status = "正在处理...";
  render();

  try {
    const summary = await invoke<RunSummary>("extract_to_xlsx", {
      inputPath: organizerState.inputPath,
      outputPath: organizerState.outputPath,
      dedupePositivePrompt: organizerState.dedupePositivePrompt,
      dedupeArtistTags: organizerState.dedupeArtistTags,
      sortByTime: organizerState.sortByTime,
      incremental: organizerState.incremental,
      imageOutputMode: organizerState.imageOutputMode
    });
    organizerState.total = summary.total_png;
    organizerState.processed = summary.processed;
    organizerState.failed = summary.failed;
    organizerState.skippedDuplicates = summary.skipped_duplicates;
    organizerState.cacheHits = summary.cache_hits;
    organizerState.processedNew = summary.processed_new;
    organizerState.warnings = summary.warnings;
    organizerState.status = `完成：已生成 ${summary.output_path}，缓存复用 ${summary.cache_hits} 张，新处理 ${summary.processed_new} 张，去重跳过 ${summary.skipped_duplicates} 张`;
  } catch (error) {
    organizerState.status = error instanceof Error ? error.message : String(error);
  } finally {
    organizerState.isRunning = false;
    organizerState.currentFile = "";
    render();
  }
});

chooseConverterInputButton.addEventListener("click", async () => {
  const selected = normalizeSelectedPath(
    await open({
      directory: false,
      multiple: false,
      filters: [{ name: "Excel 工作簿", extensions: ["xlsx"] }]
    })
  );
  if (!selected) {
    return;
  }

  const currentSequence = ++inspectionSequence;
  converterState.inputPath = selected;
  converterState.outputPath = suggestedJsonPath(selected);
  converterState.inspection = null;
  converterState.completedOutputPath = "";
  converterState.processed = 0;
  converterState.isInspecting = true;
  converterState.status = "正在检查 XLSX...";
  render();

  try {
    const inspection = await invoke<XlsxInspection>("inspect_xlsx", { inputPath: selected });
    if (currentSequence !== inspectionSequence) {
      return;
    }
    converterState.inspection = inspection;
    converterState.status = `检查完成：可转换 ${inspection.record_count} 条记录。`;
  } catch (error) {
    if (currentSequence !== inspectionSequence) {
      return;
    }
    converterState.status = error instanceof Error ? error.message : String(error);
  } finally {
    if (currentSequence === inspectionSequence) {
      converterState.isInspecting = false;
      render();
    }
  }
});

chooseConverterOutputButton.addEventListener("click", async () => {
  const selected = await save({
    defaultPath: converterState.outputPath || "zhihuiji_presets.json",
    filters: [{ name: "JSON 文件", extensions: ["json"] }]
  });
  const outputPath = normalizeSelectedPath(selected);
  if (!outputPath) {
    return;
  }
  converterState.outputPath = outputPath;
  converterState.completedOutputPath = "";
  converterState.status = converterState.inspection
    ? `已选择输出路径，可转换 ${converterState.inspection.record_count} 条记录。`
    : converterState.status;
  render();
});

startConversionButton.addEventListener("click", async () => {
  if (
    !converterState.inspection ||
    !converterState.inputPath ||
    !converterState.outputPath ||
    converterState.isRunning
  ) {
    return;
  }

  converterState.isRunning = true;
  converterState.processed = 0;
  converterState.completedOutputPath = "";
  converterState.status = "正在转换...";
  render();

  try {
    const summary = await invoke<ConversionSummary>("convert_xlsx_to_zhihuiji_json", {
      inputPath: converterState.inputPath,
      outputPath: converterState.outputPath
    });
    converterState.processed = summary.exported;
    converterState.completedOutputPath = summary.output_path;
    converterState.status = `转换完成：已导出 ${summary.exported} 条记录到 ${summary.output_path}`;
  } catch (error) {
    converterState.status = error instanceof Error ? error.message : String(error);
  } finally {
    converterState.isRunning = false;
    render();
  }
});

openOutputFolderButton.addEventListener("click", async () => {
  if (!converterState.completedOutputPath) {
    return;
  }
  try {
    await invoke("open_output_folder", { path: converterState.completedOutputPath });
  } catch (error) {
    converterState.status = error instanceof Error ? error.message : String(error);
    render();
  }
});

chooseJsonDedupeInputButton.addEventListener("click", async () => {
  const selected = normalizeSelectedPath(
    await open({
      directory: false,
      multiple: false,
      filters: [{ name: "JSON 文件", extensions: ["json"] }]
    })
  );
  if (!selected) {
    return;
  }

  const currentSequence = ++jsonDedupeInspectionSequence;
  jsonDedupeState.inputPath = selected;
  jsonDedupeState.outputPath = suggestedDedupedJsonPath(selected);
  jsonDedupeState.inspection = null;
  jsonDedupeState.completedOutputPath = "";
  jsonDedupeState.processed = 0;
  jsonDedupeState.duplicateCount = 0;
  jsonDedupeState.isInspecting = true;
  jsonDedupeState.status = "正在检查 JSON...";
  render();

  try {
    const inspection = await invoke<JsonDedupeInspection>("inspect_zhihuiji_json", {
      inputPath: selected
    });
    if (currentSequence !== jsonDedupeInspectionSequence) {
      return;
    }
    jsonDedupeState.inspection = inspection;
    jsonDedupeState.duplicateCount = inspection.duplicate_count;
    jsonDedupeState.status = `检查完成：${inspection.original_count} 条记录中有 ${inspection.duplicate_count} 条重复，预计保留 ${inspection.unique_count} 条。`;
  } catch (error) {
    if (currentSequence !== jsonDedupeInspectionSequence) {
      return;
    }
    jsonDedupeState.status = error instanceof Error ? error.message : String(error);
  } finally {
    if (currentSequence === jsonDedupeInspectionSequence) {
      jsonDedupeState.isInspecting = false;
      render();
    }
  }
});

chooseJsonDedupeOutputButton.addEventListener("click", async () => {
  const selected = await save({
    defaultPath: jsonDedupeState.outputPath || "zhihuiji_presets_deduped.json",
    filters: [{ name: "JSON 文件", extensions: ["json"] }]
  });
  const outputPath = normalizeSelectedPath(selected);
  if (!outputPath) {
    return;
  }
  jsonDedupeState.outputPath = outputPath;
  jsonDedupeState.completedOutputPath = "";
  jsonDedupeState.status = jsonDedupeState.inspection
    ? `已选择输出路径，将保留 ${jsonDedupeState.inspection.unique_count} 条记录。`
    : jsonDedupeState.status;
  render();
});

startJsonDedupeButton.addEventListener("click", async () => {
  if (
    !jsonDedupeState.inspection ||
    !jsonDedupeState.inputPath ||
    !jsonDedupeState.outputPath ||
    jsonDedupeState.isRunning
  ) {
    return;
  }

  jsonDedupeState.isRunning = true;
  jsonDedupeState.processed = 0;
  jsonDedupeState.duplicateCount = 0;
  jsonDedupeState.completedOutputPath = "";
  jsonDedupeState.status = "正在去重...";
  render();

  try {
    const summary = await invoke<JsonDedupeSummary>("dedupe_zhihuiji_json", {
      inputPath: jsonDedupeState.inputPath,
      outputPath: jsonDedupeState.outputPath
    });
    jsonDedupeState.processed = summary.original_count;
    jsonDedupeState.duplicateCount = summary.duplicate_count;
    jsonDedupeState.inspection = {
      ...jsonDedupeState.inspection,
      original_count: summary.original_count,
      duplicate_count: summary.duplicate_count,
      unique_count: summary.unique_count
    };
    jsonDedupeState.completedOutputPath = summary.output_path;
    jsonDedupeState.status = `去重完成：删除 ${summary.duplicate_count} 条，保留 ${summary.unique_count} 条，已保存到 ${summary.output_path}`;
  } catch (error) {
    jsonDedupeState.status = error instanceof Error ? error.message : String(error);
  } finally {
    jsonDedupeState.isRunning = false;
    render();
  }
});

openJsonDedupeOutputFolderButton.addEventListener("click", async () => {
  if (!jsonDedupeState.completedOutputPath) {
    return;
  }
  try {
    await invoke("open_output_folder", { path: jsonDedupeState.completedOutputPath });
  } catch (error) {
    jsonDedupeState.status = error instanceof Error ? error.message : String(error);
    render();
  }
});

await listen<ProgressPayload>("extract:start", (event) => {
  organizerState.total = event.payload.total_png ?? 0;
  organizerState.processed = 0;
  organizerState.failed = 0;
  organizerState.skippedDuplicates = 0;
  organizerState.cacheHits = 0;
  organizerState.processedNew = 0;
  organizerState.status = event.payload.message ?? "开始处理...";
  render();
});

await listen<ProgressPayload>("extract:scan_complete", (event) => {
  organizerState.total = event.payload.total_png ?? organizerState.total;
  organizerState.skippedDuplicates =
    event.payload.skipped_duplicates ?? organizerState.skippedDuplicates;
  organizerState.cacheHits = event.payload.cache_hits ?? organizerState.cacheHits;
  organizerState.processedNew = event.payload.processed_new ?? organizerState.processedNew;
  organizerState.status = event.payload.message ?? "扫描完成。";
  render();
});

await listen<ProgressPayload>("extract:file_progress", (event) => {
  organizerState.processed = event.payload.processed ?? organizerState.processed;
  organizerState.failed = event.payload.failed ?? organizerState.failed;
  organizerState.skippedDuplicates =
    event.payload.skipped_duplicates ?? organizerState.skippedDuplicates;
  organizerState.cacheHits = event.payload.cache_hits ?? organizerState.cacheHits;
  organizerState.processedNew = event.payload.processed_new ?? organizerState.processedNew;
  organizerState.currentFile = event.payload.current_file ?? "";
  organizerState.status = event.payload.message ?? "正在处理...";
  render();
});

await listen<FileWarning>("extract:file_warning", (event) => {
  organizerState.warnings.push(event.payload);
  render();
});

await listen<ConversionProgress>("convert:progress", (event) => {
  if (!converterState.isRunning) {
    return;
  }
  converterState.processed = event.payload.processed;
  converterState.status = event.payload.message;
  render();
});

await listen<JsonDedupeProgress>("json-dedupe:progress", (event) => {
  if (!jsonDedupeState.isRunning) {
    return;
  }
  jsonDedupeState.processed = event.payload.processed;
  jsonDedupeState.duplicateCount = event.payload.duplicate_count;
  jsonDedupeState.status = event.payload.message;
  render();
});

render();
