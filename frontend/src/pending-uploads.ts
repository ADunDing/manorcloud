// 未完成的上傳：伺服器上還留著、但這個分頁沒有在傳的分段上傳，顯示在右下角的面板。
// 通常來自重新整理、關閉分頁、或重試多次仍然斷線。有才出現，沒有就不顯示。
//
// 瀏覽器重新整理後不會記得使用者選過哪個檔案，所以 Resume 需要使用者再選一次同一個檔案，
// 比對相符（見 isSameFile）才從中斷處繼續。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { isSameFile, type UploadInfo } from "./chunked-upload.js";
import { confirmDialog } from "./dialogs.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { icon } from "./icons.js";
import { loadFlag, saveFlag, type UploadQueue } from "./upload-queue.js";

const MINIMIZED_KEY = "manorcloud.pending.minimized";
/** 上一次顯示的數量，記在瀏覽器中：重新整理後才知道有沒有「新的」未完成上傳 */
const SHOWN_KEY = "manorcloud.pending.shown";

export class PendingUploads {
  /** 面板，由檔案瀏覽畫面放進右下角 */
  readonly element: HTMLElement;
  private readonly count: HTMLElement;
  private readonly toggleButton: HTMLButtonElement;
  private readonly body: HTMLElement;
  private readonly list: HTMLElement;
  private minimized = loadFlag(MINIMIZED_KEY);
  /** 上一次顯示的數量：有新的未完成上傳出現時，自動展開提醒一次 */
  private shown = loadNumber(SHOWN_KEY);
  /** 載入序號：較早送出、較晚回來的回應不能蓋掉較新的結果 */
  private loading = 0;

  constructor(private readonly queue: UploadQueue) {
    this.count = h("span", { class: "panel-count" });
    this.toggleButton = h(
      "button",
      {
        type: "button",
        class: "round-button toggle",
        onClick: () => this.setMinimized(!this.minimized),
      },
      icon("chevronUp"),
    );
    this.list = h("ul", { class: "panel-list" });
    this.body = h(
      "div",
      { class: "panel-body" },
      h(
        "p",
        { class: "panel-hint" },
        "Choose the same file again to continue where it stopped. Uploads with no progress for 24 hours are deleted automatically.",
      ),
      this.list,
    );
    this.element = h(
      "section",
      { class: "panel", "aria-label": "Unfinished uploads", hidden: true },
      h(
        "div",
        { class: "panel-header" },
        h(
          "div",
          { class: "panel-heading" },
          h("strong", { class: "panel-title" }, "Unfinished uploads"),
          " ",
          this.count,
        ),
        this.toggleButton,
      ),
      this.body,
    );
    this.applyMinimized();
  }

  /** 重新向伺服器取得清單 */
  refresh(): void {
    void this.load();
  }

  private async load(): Promise<void> {
    const token = ++this.loading;
    let uploads: UploadInfo[];
    try {
      uploads = await api.get<UploadInfo[]>("/api/uploads");
    } catch {
      // 這只是輔助的清單：取得失敗就維持原狀；登入失效等由全域處理
      return;
    }
    if (token !== this.loading) {
      return;
    }

    // 這個分頁正在傳的不顯示：否則它會同時出現在兩個面板中，讓人以為它中斷了
    const active = this.queue.activeUploadIds();
    const rows = uploads.filter((info) => !active.has(info.id)).map((info) => this.row(info));
    this.list.replaceChildren(...rows);
    this.count.textContent = `(${rows.length})`;
    this.element.hidden = rows.length === 0;

    if (rows.length > this.shown && this.minimized) {
      this.setMinimized(false);
    }
    this.shown = rows.length;
    saveNumber(SHOWN_KEY, rows.length);
  }

  private row(info: UploadInfo): HTMLElement {
    const message = h("span", { class: "pending-message", role: "status" });

    // 每一列有自己的檔案選擇器：選好之後就知道是哪一個上傳要續傳
    const picker = h("input", { type: "file", hidden: true });
    picker.addEventListener("change", () => {
      const file = picker.files?.[0];
      // 清空，否則再次選擇同一個檔案時不會觸發 change
      picker.value = "";
      if (file === undefined) {
        return;
      }
      if (!isSameFile(file, info)) {
        message.textContent = "This isn't the same file: its name, size or modification time differs";
        return;
      }
      this.queue.resume(file, info);
      this.refresh();
    });

    const percent = info.size > 0 ? Math.floor((info.received / info.size) * 100) : 0;

    return h(
      "li",
      { class: "panel-item pending-item" },
      h(
        "div",
        { class: "pending-text" },
        h("span", { class: "item-name", title: info.name }, info.name),
        h(
          "span",
          { class: "pending-progress" },
          `${formatSize(info.received)} of ${formatSize(info.size)} (${percent}%)`,
        ),
      ),
      h("button", { type: "button", class: "primary", onClick: () => picker.click() }, "Resume"),
      h(
        "button",
        { type: "button", class: "link", onClick: () => this.cancel(info, message) },
        "Cancel",
      ),
      message,
      picker,
    );
  }

  private cancel(info: UploadInfo, message: HTMLElement): void {
    void (async () => {
      const confirmed = await confirmDialog({
        title: "Cancel this upload?",
        message: `The part of "${info.name}" already uploaded will be deleted.`,
        confirmLabel: "Cancel upload",
        danger: true,
      });
      if (!confirmed) {
        return;
      }
      try {
        await api.delete(`/api/uploads/${encodeURIComponent(info.id)}`);
        this.refresh();
      } catch (error) {
        if (isHandledGlobally(error)) {
          return;
        }
        // 已經不存在（例如另一個分頁完成或取消了）：一樣從清單移除
        if (error instanceof ApiError && error.status === 404) {
          this.refresh();
          return;
        }
        message.textContent = error instanceof ApiError ? error.message : "Couldn't cancel the upload";
      }
    })();
  }

  private setMinimized(minimized: boolean): void {
    this.minimized = minimized;
    saveFlag(MINIMIZED_KEY, minimized);
    this.applyMinimized();
  }

  private applyMinimized(): void {
    this.body.hidden = this.minimized;
    const label = this.minimized ? "Show unfinished uploads" : "Minimize unfinished uploads";
    this.toggleButton.setAttribute("aria-expanded", String(!this.minimized));
    this.toggleButton.setAttribute("aria-label", label);
    this.toggleButton.title = label;
  }
}

function loadNumber(key: string): number {
  try {
    const value = Number(localStorage.getItem(key));
    return Number.isSafeInteger(value) && value > 0 ? value : 0;
  } catch {
    return 0;
  }
}

function saveNumber(key: string, value: number): void {
  try {
    localStorage.setItem(key, String(value));
  } catch {
    // 無法儲存只代表下次可能多展開一次
  }
}
