// 檔案瀏覽：麵包屑導覽、工具列、上傳佇列、檔案列表。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { formatSize, formatTime } from "./format.js";
import { folderHref } from "./router.js";
import type { Crumb, FileItem, ItemDetail, ServerConfig } from "./types.js";
import { PendingUploads } from "./pending-uploads.js";
import { UploadQueue } from "./upload-queue.js";

/**
 * 導覽序號。每次載入資料夾時遞增，回應到達時比對：不是最新的就丟棄。
 * 否則使用者快速點了 A 再點 B，若 A 的回應較晚到，畫面會停在 A，網址卻顯示 B。
 */
let navigation = 0;

/** 目前顯示的資料夾。上傳完成時，只有在使用者正在看那個資料夾時才重新載入 */
let current: { container: HTMLElement; folderId: number | null } | null = null;

/** 上傳上限：第一次需要時向伺服器取得，之後重複使用 */
let limitPromise: Promise<number | null> | null = null;

function uploadLimit(): Promise<number | null> {
  limitPromise ??= api
    .get<ServerConfig>("/api/config")
    .then((config) => config.max_upload_bytes)
    .catch(() => {
      // 取得失敗就不在前端預先檢查，交給後端；下次再重試
      limitPromise = null;
      return null;
    });
  return limitPromise;
}

/** 上傳佇列在整個應用程式中只有一個，切換資料夾時保留 */
const queue = new UploadQueue(
  (folderId) => {
    if (current !== null && current.folderId === folderId) {
      renderBrowser(current.container, current.folderId);
    }
  },
  uploadLimit,
  // 分段上傳結束（完成、失敗或取消）：未完成的清單可能變了。
  // 這個函式在上傳結束時才會被呼叫，那時 pending 早已建立
  () => pending.refresh(),
);

/** 未完成的上傳清單，與佇列一樣在整個應用程式中只有一個 */
const pending = new PendingUploads(queue);

// 上傳進行中關閉分頁或重新整理，上傳會直接中斷。伺服器會清乾淨，不會有資料問題，
// 但使用者可能不知道還有檔案沒傳完。呼叫 preventDefault 後，瀏覽器會跳出內建的確認視窗。
// 視窗的文字由瀏覽器決定，網頁無法自訂（避免被濫用來欺騙使用者）。
window.addEventListener("beforeunload", (event) => {
  if (queue.isBusy()) {
    event.preventDefault();
  }
});

/** 在 container 中顯示資料夾的內容。folderId 為 null 代表根目錄 */
export function renderBrowser(
  container: HTMLElement,
  folderId: number | null,
): void {
  current = { container, folderId };
  void load(container, folderId);
}

async function load(
  container: HTMLElement,
  folderId: number | null,
): Promise<void> {
  const token = ++navigation;
  container.replaceChildren(h("p", { class: "muted" }, "載入中…"));

  try {
    // 列表與麵包屑同時請求
    const [items, detail] = await Promise.all([
      api.get<FileItem[]>(
        folderId === null ? "/api/files" : `/api/files?parent_id=${folderId}`,
      ),
      folderId === null
        ? Promise.resolve(null)
        : api.get<ItemDetail>(`/api/files/${folderId}`),
    ]);

    // 等待期間使用者已經切換到別的資料夾：這份結果已經過時
    if (token !== navigation) {
      return;
    }

    const errorBox = h("p", { class: "error", role: "alert" });
    const view = new FolderView(container, folderId, errorBox);

    container.replaceChildren(
      breadcrumbs(detail),
      view.toolbar(),
      errorBox,
      // 同一個元素被放回來：上傳的進度與狀態都保留
      pending.element,
      queue.element,
      view.dropZone(items),
    );
    // 未完成的清單不屬於任何資料夾，但每次載入時更新，使用者總是看到最新的狀態
    pending.refresh();
  } catch (error) {
    if (token !== navigation || isHandledGlobally(error)) {
      return;
    }
    container.replaceChildren(
      h("p", { class: "error", role: "alert" }, messageOf(error)),
      h("p", {}, h("a", { href: folderHref(null) }, "回到我的檔案")),
    );
  }
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "發生未預期的錯誤";
}

// ==================== 麵包屑 ====================

/** 我的檔案 / 相片 / 2026。目前所在的那一層不是連結 */
function breadcrumbs(detail: ItemDetail | null): HTMLElement {
  const crumbs: Crumb[] = detail
    ? [...detail.ancestors, { id: detail.id, name: detail.name }]
    : [];

  const root =
    crumbs.length === 0
      ? h("span", { "aria-current": "page" }, "我的檔案")
      : h("a", { href: folderHref(null) }, "我的檔案");

  const items = crumbs.map((crumb, index) =>
    h(
      "li",
      {},
      index === crumbs.length - 1
        ? h("span", { "aria-current": "page" }, crumb.name)
        : h("a", { href: folderHref(crumb.id) }, crumb.name),
    ),
  );

  return h(
    "nav",
    { class: "breadcrumbs", "aria-label": "目前位置" },
    h("ol", {}, h("li", {}, root), items),
  );
}

// ==================== 資料夾的操作 ====================

/** 一個資料夾畫面上的操作：上傳、建立、改名、刪除 */
class FolderView {
  /** 操作進行中，避免重複送出（例如連點兩次刪除） */
  private busy = false;

  constructor(
    private readonly container: HTMLElement,
    private readonly folderId: number | null,
    private readonly errorBox: HTMLElement,
  ) {}

  toolbar(): HTMLElement {
    // 隱藏的檔案選擇器，由「上傳檔案」按鈕觸發。
    // 保留按鈕而不是只靠拖放：鍵盤與觸控使用者也能上傳
    const picker = h("input", { type: "file", multiple: true, hidden: true });
    picker.addEventListener("change", () => {
      if (picker.files !== null && picker.files.length > 0) {
        queue.add(Array.from(picker.files), this.folderId);
      }
      // 清空，否則再次選擇同一個檔案時不會觸發 change
      picker.value = "";
    });

    return h(
      "div",
      { class: "toolbar" },
      h(
        "button",
        { type: "button", onClick: () => picker.click() },
        "上傳檔案",
      ),
      h(
        "button",
        { type: "button", onClick: () => this.createFolder() },
        "新增資料夾",
      ),
      picker,
    );
  }

  /** 檔案列表，同時是拖放上傳的區域 */
  dropZone(items: readonly FileItem[]): HTMLElement {
    const zone = h(
      "div",
      { class: "dropzone" },
      items.length === 0
        ? h(
            "p",
            { class: "muted empty" },
            "這個資料夾是空的。可以把檔案拖放到這裡上傳。",
          )
        : this.table(items),
    );

    zone.addEventListener("dragover", (event) => {
      // 必須取消預設行為，瀏覽器才會允許在這裡放下
      event.preventDefault();
      zone.classList.add("dragging");
    });
    zone.addEventListener("dragleave", () => zone.classList.remove("dragging"));
    zone.addEventListener("drop", (event) => {
      event.preventDefault();
      zone.classList.remove("dragging");
      this.handleDrop(event);
    });

    return zone;
  }

  /**
   * 處理放下的項目。瀏覽器會把拖進來的資料夾也當成一個「檔案」，
   * 送出時才會失敗，錯誤訊息還會誤導成「無法連線」，所以在這裡先排除並說明。
   */
  private handleDrop(event: DragEvent): void {
    const transfer = event.dataTransfer;
    if (transfer === null) {
      return;
    }

    const files: File[] = [];
    const folders: string[] = [];
    for (const item of Array.from(transfer.items)) {
      if (item.kind !== "file") {
        continue;
      }
      const entry = item.webkitGetAsEntry();
      if (entry?.isDirectory) {
        folders.push(entry.name);
        continue;
      }
      const file = item.getAsFile();
      if (file !== null) {
        files.push(file);
      }
    }

    this.errorBox.textContent =
      folders.length > 0 ? `目前不支援上傳資料夾：${folders.join("、")}` : "";
    if (files.length > 0) {
      queue.add(files, this.folderId);
    }
  }

  private table(items: readonly FileItem[]): HTMLElement {
    return h(
      "table",
      { class: "files" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", { scope: "col" }, "名稱"),
          h("th", { scope: "col", class: "num" }, "大小"),
          h("th", { scope: "col" }, "修改時間"),
          h(
            "th",
            { scope: "col" },
            h("span", { class: "visually-hidden" }, "操作"),
          ),
        ),
      ),
      h(
        "tbody",
        {},
        items.map((item) => this.row(item)),
      ),
    );
  }

  private row(item: FileItem): HTMLElement {
    const isFolder = item.kind === "folder";

    // 名稱：資料夾是連結，點了進入；檔案是純文字
    const name = isFolder
      ? h("a", { href: folderHref(item.id) }, icon("📁"), item.name)
      : h("span", {}, icon("📄"), item.name);

    return h(
      "tr",
      {},
      h("td", { class: "name" }, name),
      h("td", { class: "num" }, formatSize(item.size)),
      h("td", {}, formatTime(item.updated_at)),
      h(
        "td",
        { class: "actions-cell" },
        // 下載：伺服器回 Content-Disposition: attachment，瀏覽器會存檔而不會離開這個頁面
        isFolder
          ? null
          : h("a", { href: `/api/files/${item.id}/content` }, "下載"),
        h(
          "button",
          { type: "button", class: "link", onClick: () => this.rename(item) },
          "改名",
        ),
        h(
          "button",
          { type: "button", class: "link", onClick: () => this.remove(item) },
          "刪除",
        ),
      ),
    );
  }

  private createFolder(): void {
    const name = window.prompt("新資料夾的名稱");
    if (name === null || name === "") {
      return;
    }
    this.perform(() =>
      api.post("/api/folders", { name, parent_id: this.folderId }),
    );
  }

  private rename(item: FileItem): void {
    const name = window.prompt("新的名稱", item.name);
    if (name === null || name === "" || name === item.name) {
      return;
    }
    this.perform(() => api.patch(`/api/files/${item.id}`, { name }));
  }

  private remove(item: FileItem): void {
    // confirm 的文字是純文字，檔名不會被當成 HTML 解析
    const message =
      item.kind === "folder"
        ? `確定要刪除資料夾「${item.name}」以及其中所有內容嗎？此動作無法復原。`
        : `確定要刪除「${item.name}」嗎？此動作無法復原。`;
    if (!window.confirm(message)) {
      return;
    }
    this.perform(() => api.delete(`/api/files/${item.id}`));
  }

  /** 執行一個操作：成功後重新載入，失敗則顯示錯誤 */
  private perform(action: () => Promise<unknown>): void {
    if (this.busy) {
      return;
    }
    this.busy = true;
    this.errorBox.textContent = "";

    void (async () => {
      try {
        await action();
        renderBrowser(this.container, this.folderId);
      } catch (error) {
        if (!isHandledGlobally(error)) {
          this.errorBox.textContent = messageOf(error);
        }
      } finally {
        this.busy = false;
      }
    })();
  }
}

/** 裝飾用的圖示：對螢幕閱讀器隱藏，避免念出「資料夾圖示」 */
function icon(symbol: string): HTMLElement {
  return h("span", { class: "icon", "aria-hidden": "true" }, symbol);
}
