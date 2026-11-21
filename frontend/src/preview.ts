// 檔案預覽：圖片、影片、音樂、PDF、文字檔。
//
// - 圖片：伺服器縮小、轉成 JPEG 的預覽圖（GET /api/files/thumbnail），不直接顯示原始檔案。
//   網址帶上檔案的大小與修改時間（v=）：檔案改變時網址就不同，瀏覽器可以放心快取
// - 影片、音樂：<video>、<audio> 直接播放下載的網址（支援 Range，可以拖曳進度）。
//   瀏覽器不支援的編碼（例如部分 MKV、HEVC）會顯示說明，可以下載
// - PDF：pdf.js（public/vendor/pdfjs，預覽 PDF 時才載入）畫在 <canvas> 上，捲到哪一頁才畫哪一頁
// - 文字檔：讀取前 1 MB，以純文字顯示（textContent，不會被當成 HTML）
//
// 預覽是全螢幕的 <dialog>：← → 切換同一個資料夾中可以預覽的檔案，Esc 關閉，可以下載原始檔案。

import { t } from "./i18n.js";
import { h } from "./dom.js";
import { icon } from "./icons.js";
import { apiUrl, type Space } from "./space.js";
import type { FileItem } from "./types.js";

export type PreviewKind = "image" | "video" | "audio" | "pdf" | "text";

const KINDS: Readonly<Record<PreviewKind, readonly string[]>> = {
  image: ["jpg", "jpeg", "png", "gif", "webp", "bmp"],
  video: ["mp4", "m4v", "mov", "webm", "mkv"],
  audio: ["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus"],
  pdf: ["pdf"],
  text: [
    "txt", "md", "markdown", "log", "csv", "tsv", "srt", "vtt", "ass", "nfo",
    "json", "yml", "yaml", "toml", "ini", "conf", "cfg", "env", "xml", "html", "htm", "css",
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "rs", "py", "go", "java", "kt", "c", "h", "cpp", "hpp",
    "cs", "rb", "php", "swift", "lua", "sh", "bash", "zsh", "ps1", "bat", "sql", "gitignore",
  ],
};

/** 沒有副檔名、但一定是文字的檔名 */
const TEXT_NAMES: ReadonlySet<string> = new Set(["dockerfile", "makefile", "license", "readme"]);

const BY_EXTENSION: ReadonlyMap<string, PreviewKind> = new Map(
  (Object.keys(KINDS) as PreviewKind[]).flatMap((kind) => KINDS[kind].map((ext) => [ext, kind] as const)),
);

/** 文字檔最多讀取多少位元組 */
const TEXT_LIMIT = 1024 * 1024;

/** 可以預覽的種類；不能預覽時是 null */
export function previewKind(item: FileItem): PreviewKind | null {
  if (item.kind !== "file") {
    return null;
  }
  const lower = item.name.toLowerCase();
  if (TEXT_NAMES.has(lower)) {
    return "text";
  }
  const dot = lower.lastIndexOf(".");
  return dot < 0 ? null : (BY_EXTENSION.get(lower.slice(dot + 1)) ?? null);
}

export function isImage(item: FileItem): boolean {
  return previewKind(item) === "image";
}

/** 縮圖（small，檔案列表）或預覽圖（large）的網址 */
export function thumbnailUrl(space: Space, path: string, item: FileItem, size: "small" | "large"): string {
  return apiUrl("/api/files/thumbnail", space, {
    path,
    size,
    v: `${item.size ?? 0}-${item.updated_at}`,
  });
}

/** 預覽中的一個檔案 */
export interface PreviewItem {
  readonly item: FileItem;
  /** 空間中的路徑 */
  readonly path: string;
}

/**
 * 開啟預覽。
 * @param items      可以切換的檔案（目前資料夾中可以預覽的檔案，依列表的順序）
 * @param start      一開始顯示第幾個
 * @param onDownload 按下 Download：下載原始檔案
 */
export function openPreview(
  space: Space,
  items: readonly PreviewItem[],
  start: number,
  onDownload: (item: PreviewItem) => void,
): void {
  if (items.length === 0) {
    return;
  }
  let index = Math.min(Math.max(start, 0), items.length - 1);
  /** 目前顯示的內容；切換或關閉時呼叫它的 stop 停止播放、釋放資源 */
  let current: Shown | null = null;

  const title = h("h2", { class: "viewer-title" });
  const counter = h("span", { class: "viewer-counter" });
  const stage = h("div", { class: "viewer-stage" });
  const previous = navButton(t("Previous"), "chevronLeft", () => go(-1));
  const next = navButton(t("Next"), "chevronRight", () => go(1));
  const close = h(
    "button",
    { type: "button", class: "viewer-button", "aria-label": t("Close"), title: t("Close (Esc)") },
    icon("close"),
  );
  const download = h(
    "button",
    { type: "button", class: "viewer-button", "aria-label": t("Download"), title: t("Download the original") },
    icon("download"),
  );

  const dialog = h(
    "dialog",
    { class: "viewer", "aria-label": t("Preview") },
    h("header", { class: "viewer-header" }, title, counter, download, close),
    h("div", { class: "viewer-body" }, stage, previous, next),
  );

  function show(): void {
    const entry = items[index];
    if (entry === undefined) {
      return;
    }
    current?.stop();
    title.textContent = entry.item.name;
    title.title = entry.item.name;
    counter.textContent = `${index + 1} / ${items.length}`;
    counter.hidden = previous.hidden = next.hidden = items.length <= 1;
    current = render(space, entry, stage);
    // 先載入前後兩張圖片，切換時不用等
    for (const near of [index - 1, index + 1]) {
      const other = items[near];
      if (other !== undefined && previewKind(other.item) === "image") {
        new Image().src = thumbnailUrl(space, other.path, other.item, "large");
      }
    }
  }

  function go(step: number): void {
    index = (index + step + items.length) % items.length;
    show();
  }

  close.addEventListener("click", () => dialog.close());
  download.addEventListener("click", () => {
    const entry = items[index];
    if (entry !== undefined) {
      onDownload(entry);
    }
  });
  dialog.addEventListener("keydown", (event) => {
    // 播放器、文字區有自己的方向鍵用法
    if (event.target instanceof HTMLMediaElement || event.target instanceof HTMLTextAreaElement) {
      return;
    }
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      go(-1);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      go(1);
    }
  });
  // 點圖片以外的黑色區域也關閉
  dialog.addEventListener("click", (event) => {
    if (event.target === dialog || event.target === stage) {
      dialog.close();
    }
  });
  dialog.addEventListener("close", () => {
    current?.stop();
    dialog.remove();
  });

  document.body.append(dialog);
  dialog.showModal();
  close.focus();
  show();
}

interface Shown {
  stop(): void;
}

/** 在 stage 中顯示一個檔案 */
function render(space: Space, entry: PreviewItem, stage: HTMLElement): Shown {
  const kind = previewKind(entry.item);
  const contentUrl = apiUrl("/api/files/content", space, { path: entry.path });
  const status = h("p", { class: "viewer-status", role: "status" }, t("Loading…"));
  const fail = (message: string): void => {
    stage.className = "viewer-stage";
    stage.replaceChildren(h("p", { class: "viewer-status", role: "status" }, message));
  };
  const cantShow = t("This file can't be shown here. Download it to open it on your device.");
  stage.replaceChildren(status);
  stage.className = `viewer-stage ${kind ?? ""}`;

  switch (kind) {
    case "image": {
      const image = h("img", { class: "viewer-image", alt: entry.item.name, hidden: true });
      image.addEventListener("load", () => {
        status.remove();
        image.hidden = false;
      });
      image.addEventListener("error", () => fail(cantShow));
      image.src = thumbnailUrl(space, entry.path, entry.item, "large");
      stage.append(image);
      return { stop: () => image.removeAttribute("src") };
    }
    case "video":
    case "audio": {
      const media = h(kind, {
        class: `viewer-${kind}`,
        controls: true,
        autoplay: true,
        preload: "metadata",
        src: contentUrl,
      });
      media.addEventListener("loadedmetadata", () => status.remove());
      media.addEventListener("error", () =>
        fail(t("This browser can't play this file. Download it to play it on your device.")),
      );
      stage.append(media);
      return {
        stop: () => {
          media.pause();
          media.removeAttribute("src");
          media.load();
        },
      };
    }
    case "pdf":
      return renderPdf(contentUrl, stage, status, () => fail(cantShow));
    case "text": {
      const controller = new AbortController();
      void (async () => {
        try {
          const response = await fetch(contentUrl, {
            headers: { Range: `bytes=0-${TEXT_LIMIT - 1}` },
            signal: controller.signal,
          });
          if (!response.ok) {
            fail(cantShow);
            return;
          }
          const bytes = new Uint8Array(await response.arrayBuffer());
          // 有 NUL 字元的不是文字檔
          if (bytes.subarray(0, 8192).includes(0)) {
            fail(cantShow);
            return;
          }
          const truncated = (entry.item.size ?? 0) > bytes.length;
          stage.replaceChildren(
            h(
              "div",
              { class: "viewer-text-wrap" },
              h("pre", { class: "viewer-text", tabindex: "0" }, new TextDecoder().decode(bytes)),
              truncated ? h("p", { class: "viewer-note" }, t("Only the first 1 MB is shown. Download the file to see all of it.")) : null,
            ),
          );
        } catch (error) {
          if (!controller.signal.aborted) {
            fail(cantShow);
          }
        }
      })();
      return { stop: () => controller.abort() };
    }
    default:
      fail(cantShow);
      return { stop: () => undefined };
  }
}

// ==================== PDF ====================

interface PdfViewport {
  readonly width: number;
  readonly height: number;
}

interface PdfPage {
  getViewport(options: { scale: number }): PdfViewport;
  render(options: { canvas: HTMLCanvasElement; viewport: PdfViewport }): { promise: Promise<void> };
}

interface PdfDocument {
  readonly numPages: number;
  getPage(number: number): Promise<PdfPage>;
}

interface PdfLoadingTask {
  readonly promise: Promise<PdfDocument>;
  destroy(): Promise<void>;
}

interface PdfJs {
  GlobalWorkerOptions: { workerSrc: string };
  getDocument(options: Record<string, unknown>): PdfLoadingTask;
}

const PDFJS = "/vendor/pdfjs/";
let pdfjs: Promise<PdfJs> | null = null;

function loadPdfJs(): Promise<PdfJs> {
  pdfjs ??= (import(`${PDFJS}pdf.min.mjs`) as Promise<PdfJs>).then((lib) => {
    lib.GlobalWorkerOptions.workerSrc = `${PDFJS}pdf.worker.min.mjs`;
    return lib;
  });
  return pdfjs;
}

/** PDF 一頁的最大寬度（CSS 像素） */
const PDF_MAX_WIDTH = 1000;

function renderPdf(url: string, stage: HTMLElement, status: HTMLElement, fail: () => void): Shown {
  let task: PdfLoadingTask | null = null;
  let observer: IntersectionObserver | null = null;
  let stopped = false;

  void (async () => {
    try {
      const lib = await loadPdfJs();
      if (stopped) {
        return;
      }
      task = lib.getDocument({
        url,
        cMapUrl: `${PDFJS}cmaps/`,
        cMapPacked: true,
        standardFontDataUrl: `${PDFJS}standard_fonts/`,
        iccUrl: `${PDFJS}iccs/`,
        wasmUrl: `${PDFJS}wasm/`,
        useWasm: false,
        enableXfa: false,
      });
      const pdf = await task.promise;
      const first = await pdf.getPage(1);
      if (stopped) {
        return;
      }
      const pages = h("div", { class: "viewer-pdf", tabindex: "0" });
      const width = Math.min(PDF_MAX_WIDTH, stage.clientWidth - 16);
      const base = first.getViewport({ scale: 1 });
      // 還沒畫的頁面先用第一頁的比例佔位，捲動列才正確
      const placeholders = Array.from({ length: pdf.numPages }, (_, i) => {
        const page = h("div", { class: "viewer-pdf-page", "data-page": String(i + 1) });
        page.style.width = `${width}px`;
        page.style.height = `${(width * base.height) / base.width}px`;
        return page;
      });
      pages.append(...placeholders);
      status.remove();
      stage.replaceChildren(pages);

      const drawn = new Set<number>();
      observer = new IntersectionObserver(
        (entries) => {
          for (const entry of entries) {
            const number = Number((entry.target as HTMLElement).dataset["page"]);
            if (!entry.isIntersecting || drawn.has(number)) {
              continue;
            }
            drawn.add(number);
            void draw(pdf, number, entry.target as HTMLElement, width);
          }
        },
        { root: pages, rootMargin: "100% 0px" },
      );
      for (const page of placeholders) {
        observer.observe(page);
      }
    } catch {
      if (!stopped) {
        fail();
      }
    }
  })();

  async function draw(pdf: PdfDocument, number: number, holder: HTMLElement, width: number): Promise<void> {
    try {
      const page = await pdf.getPage(number);
      const scale = width / page.getViewport({ scale: 1 }).width;
      const ratio = window.devicePixelRatio || 1;
      const viewport = page.getViewport({ scale: scale * ratio });
      const canvas = h("canvas", { width: Math.floor(viewport.width), height: Math.floor(viewport.height) });
      canvas.style.width = `${width}px`;
      holder.style.height = `${viewport.height / ratio}px`;
      holder.replaceChildren(canvas);
      await page.render({ canvas, viewport }).promise;
    } catch {
      // 一頁畫不出來不影響其他頁
    }
  }

  return {
    stop: () => {
      stopped = true;
      observer?.disconnect();
      void task?.destroy();
    },
  };
}

function navButton(label: string, name: "chevronLeft" | "chevronRight", onClick: () => void): HTMLButtonElement {
  return h(
    "button",
    {
      type: "button",
      class: `viewer-nav ${name === "chevronLeft" ? "previous" : "next"}`,
      "aria-label": label,
      title: label,
      onClick: (event: Event) => {
        event.stopPropagation();
        onClick();
      },
    },
    icon(name, 28),
  );
}
