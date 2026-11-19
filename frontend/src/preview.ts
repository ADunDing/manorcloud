// 圖片的縮圖與預覽。
//
// 縮圖與預覽圖都由伺服器縮小、轉成 JPEG（GET /api/files/thumbnail），不直接顯示原始檔案。
// 網址帶上檔案的大小與修改時間（v=）：檔案改變時網址就不同，瀏覽器可以放心快取。
//
// 預覽是全螢幕的 <dialog>：← → 切換同一個資料夾中的圖片，Esc 關閉，可以下載原始檔案。

import { h } from "./dom.js";
import { icon } from "./icons.js";
import { apiUrl, type Space } from "./space.js";
import type { FileItem } from "./types.js";

/** 伺服器能產生縮圖的格式（依副檔名判斷；不是真的圖片時伺服器回 404，畫面改顯示一般圖示） */
const IMAGE_EXTENSIONS: ReadonlySet<string> = new Set(["jpg", "jpeg", "png", "gif", "webp", "bmp"]);

export function isImage(item: FileItem): boolean {
  if (item.kind !== "file") {
    return false;
  }
  const dot = item.name.lastIndexOf(".");
  return dot > 0 && IMAGE_EXTENSIONS.has(item.name.slice(dot + 1).toLowerCase());
}

/** 縮圖（small，檔案列表）或預覽圖（large）的網址 */
export function thumbnailUrl(space: Space, path: string, item: FileItem, size: "small" | "large"): string {
  return apiUrl("/api/files/thumbnail", space, {
    path,
    size,
    v: `${item.size ?? 0}-${item.updated_at}`,
  });
}

/** 預覽中的一張圖片 */
export interface PreviewItem {
  readonly item: FileItem;
  /** 空間中的路徑 */
  readonly path: string;
}

/**
 * 開啟預覽。
 * @param items     可以切換的圖片（目前資料夾中的圖片，依列表的順序）
 * @param start     一開始顯示第幾張
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
  const urlOf = (i: number): string => {
    const entry = items[i];
    return entry === undefined ? "" : thumbnailUrl(space, entry.path, entry.item, "large");
  };

  const title = h("h2", { class: "viewer-title" });
  const counter = h("span", { class: "viewer-counter" });
  const status = h("p", { class: "viewer-status", role: "status" });
  const image = h("img", { class: "viewer-image", alt: "" });
  const previous = navButton("Previous", "chevronLeft", () => go(-1));
  const next = navButton("Next", "chevronRight", () => go(1));
  const close = h(
    "button",
    { type: "button", class: "viewer-button", "aria-label": "Close", title: "Close (Esc)" },
    icon("close"),
  );
  const download = h(
    "button",
    { type: "button", class: "viewer-button", "aria-label": "Download", title: "Download the original" },
    icon("download"),
  );

  const dialog = h(
    "dialog",
    { class: "viewer", "aria-label": "Preview" },
    h("header", { class: "viewer-header" }, title, counter, download, close),
    h("div", { class: "viewer-stage" }, image, status, previous, next),
  );

  image.addEventListener("load", () => {
    status.textContent = "";
    image.hidden = false;
  });
  image.addEventListener("error", () => {
    image.hidden = true;
    status.textContent = "This picture can't be shown. Download it to open it on your device.";
  });

  function show(): void {
    const entry = items[index];
    if (entry === undefined) {
      return;
    }
    title.textContent = entry.item.name;
    title.title = entry.item.name;
    counter.textContent = items.length > 1 ? `${index + 1} / ${items.length}` : "";
    counter.hidden = items.length <= 1;
    previous.hidden = next.hidden = items.length <= 1;
    image.hidden = true;
    status.textContent = "Loading…";
    image.alt = entry.item.name;
    image.src = urlOf(index);
    // 先載入前後兩張，切換時不用等
    for (const near of [index - 1, index + 1]) {
      if (near >= 0 && near < items.length) {
        new Image().src = urlOf(near);
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
    if (event.target === dialog || (event.target as HTMLElement).classList.contains("viewer-stage")) {
      dialog.close();
    }
  });
  dialog.addEventListener("close", () => dialog.remove());

  document.body.append(dialog);
  dialog.showModal();
  close.focus();
  show();
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
