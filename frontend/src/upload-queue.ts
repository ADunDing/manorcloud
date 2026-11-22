// 上傳佇列：依序上傳檔案，進度顯示在右下角的面板。
//
// 面板的三種狀態：
// - 上傳中：標題「Uploading… (23%)」，總進度、速度、剩餘時間，以及每個檔案
// - 全部成功：標題「Uploads complete」，幾秒後自動消失
// - 有失敗：標題「1 upload failed」，失敗的項目留著顯示原因，使用者按關閉才消失
//   （失敗的訊息如果一閃即逝，使用者會以為檔案已經傳好了）。
//   失敗的項目一律排在清單最上方的「Failed」一區；全部結束後只留下失敗的項目，
//   一次上傳很多檔案時不必在一長串完成的檔案中尋找
// 使用者取消的檔案直接從清單移除，不算失敗。
//
// 佇列是獨立存在的物件，自己持有面板的元素，切換資料夾時面板與進度都保留。

import { t } from "./i18n.js";
import { api, ApiError, isHandledGlobally } from "./api.js";
import { withSpace, type Space } from "./space.js";
import {
  CHUNKED_THRESHOLD,
  isSameFile,
  startChunkedUpload,
  type UploadInfo,
  whenDiscarded,
} from "./chunked-upload.js";
import { h } from "./dom.js";
import { formatDuration, formatSize } from "./format.js";
import { icon } from "./icons.js";
import { startUpload, UploadCancelled, type UploadHandle } from "./upload.js";

type Status = "pending" | "uploading" | "done" | "failed";

/** 佇列中的一個檔案。畫面上的元素也存在這裡，更新進度時只改這一列 */
interface Entry {
  readonly file: File;
  /** 上傳的目標資料夾（例如 相片/2026）：使用者中途切換資料夾，檔案仍然傳到原本的地方 */
  readonly dir: string;
  /** 目標資料夾所在的空間（見 space.ts），同樣在加入時就決定 */
  readonly space: Space;
  /** 要續傳的伺服器上的上傳（從未完成的清單按 Resume 時） */
  readonly resume: UploadInfo | null;
  status: Status;
  handle: UploadHandle | null;
  /** 分段上傳的編號：未完成的清單用它排除這個分頁正在傳的上傳 */
  uploadId: string | null;
  /** 已傳的比例，0～1 */
  fraction: number;
  readonly row: HTMLElement;
  readonly fill: HTMLElement;
  readonly statusText: HTMLElement;
  readonly sizeText: HTMLElement;
  readonly cancelButton: HTMLButtonElement;
}

const MINIMIZED_KEY = "manorcloud.uploads.minimized";
/** 全部成功後，面板停留多久才消失 */
const HIDE_AFTER_MS = 3000;
/** 速度的平滑程度：新的量測佔多少比重。數字越小，顯示的速度越不會跳動 */
const SPEED_SMOOTHING = 0.3;

export class UploadQueue {
  /** 面板，由檔案瀏覽畫面放進右下角 */
  readonly element: HTMLElement;
  private readonly title: HTMLElement;
  private readonly count: HTMLElement;
  private readonly closeButton: HTMLButtonElement;
  private readonly toggleButton: HTMLButtonElement;
  private readonly body: HTMLElement;
  private readonly summary: HTMLElement;
  private readonly summaryFill: HTMLElement;
  private readonly statBytes: HTMLElement;
  private readonly statSpeed: HTMLElement;
  private readonly statLeft: HTMLElement;
  private readonly list: HTMLElement;
  private entries: Entry[] = [];
  /**
   * 已取消、伺服器正在刪除的分段上傳。刪除要一點時間：這段期間它還在伺服器的清單裡，
   * 但不該被顯示成「未完成」，否則使用者會以為取消的檔案還可以續傳，過一會兒它又自己消失
   */
  private readonly cancelledIds = new Set<string>();
  private running = false;
  private minimized = loadFlag(MINIMIZED_KEY);
  private hideTimer: number | undefined;
  private ticker: number | undefined;
  private lastSample: { time: number; bytes: number } | null = null;
  /** 每秒位元組數；0 代表還不知道 */
  private speed = 0;

  /**
   * @param onUploaded 一個檔案上傳完成時呼叫，傳入它的目標資料夾與檔案空間（用來決定要不要重新載入列表）
   * @param uploadLimit 取得上傳上限；無法取得時回傳 null，此時不在前端預先檢查
   * @param onChunkedSettled 一個分段上傳結束時呼叫（完成、失敗或取消）：未完成的清單可能變了
   */
  constructor(
    private readonly onUploaded: (dir: string, space: Space) => void,
    private readonly uploadLimit: () => Promise<number | null>,
    private readonly onChunkedSettled: () => void,
  ) {
    this.title = h("strong", { class: "panel-title" });
    this.count = h("span", { class: "panel-count" });
    this.closeButton = h(
      "button",
      { type: "button", class: "round-button", onClick: () => this.closeOrCancel() },
      icon("close", 16),
    );
    this.toggleButton = h(
      "button",
      {
        type: "button",
        class: "round-button toggle",
        onClick: () => this.setMinimized(!this.minimized),
      },
      icon("chevronUp"),
    );

    this.summaryFill = h("div", { class: "bar-fill" });
    this.statBytes = h("span", {});
    this.statSpeed = h("span", {});
    this.statLeft = h("span", {});
    this.summary = h(
      "div",
      { class: "panel-summary" },
      h("div", { class: "bar" }, this.summaryFill),
      h("div", { class: "panel-stats" }, this.statBytes, this.statSpeed, this.statLeft),
    );
    this.list = h("ul", { class: "panel-list" });
    this.body = h("div", { class: "panel-body" }, this.summary, this.list);

    this.element = h(
      "section",
      { class: "panel", "aria-label": t("Uploads"), hidden: true },
      h(
        "div",
        { class: "panel-header" },
        h("div", { class: "panel-heading" }, this.title, " ", this.count),
        this.closeButton,
        this.toggleButton,
      ),
      this.body,
    );
    this.applyMinimized();
  }

  /** 加入檔案並開始依序上傳 */
  add(files: readonly File[], dir: string, space: Space = null): void {
    for (const file of files) {
      this.entries.push(this.createEntry(file, dir, space, null));
    }
    this.renderList();
    void this.run();
  }

  /** 續傳伺服器上未完成的上傳：傳回原本的資料夾，不是目前所在的資料夾 */
  resume(file: File, info: UploadInfo, space: Space = null): void {
    this.entries.push(this.createEntry(file, info.path, space, info));
    this.renderList();
    void this.run();
  }

  /** 是否還有檔案正在上傳或等待上傳 */
  isBusy(): boolean {
    return this.entries.some(isActive);
  }

  /** 這個分頁正在傳、等待續傳、或剛取消還在刪除中的分段上傳編號：未完成的清單不顯示它們 */
  activeUploadIds(): Set<string> {
    const ids = new Set<string>(this.cancelledIds);
    for (const entry of this.entries) {
      if (isActive(entry) && entry.uploadId !== null) {
        ids.add(entry.uploadId);
      }
    }
    return ids;
  }

  // ==================== 執行 ====================

  /**
   * 依序上傳：一次只傳一個。樹莓派的 CPU 與磁碟寫入速度有限，
   * 同時處理多個大檔案反而更慢，進度也比較好呈現。
   */
  private async run(): Promise<void> {
    if (this.running) {
      return;
    }
    this.running = true;
    try {
      for (;;) {
        const next = this.entries.find((entry) => entry.status === "pending");
        if (next === undefined) {
          break;
        }
        await this.uploadOne(next);
      }
    } finally {
      this.running = false;
    }
  }

  private async uploadOne(entry: Entry): Promise<void> {
    // 送出前先檢查大小：不必傳到一半才收到 413。後端的檢查仍是唯一的權威
    const limit = await this.uploadLimit();
    if (limit !== null && entry.file.size > limit) {
      this.setStatus(entry, "failed", t("Larger than the {limit} limit", { limit: formatSize(limit) }));
      return;
    }

    this.setStatus(entry, "uploading", t("Uploading 0%"));
    const onProgress = (fraction: number): void => this.setProgress(entry, fraction);
    // 重試等提示暫時取代進度文字；清除時恢復顯示目前的百分比
    const onNotice = (message: string | null): void => {
      entry.statusText.textContent = message ?? progressText(entry.fraction);
    };

    // 大檔案分段上傳：斷線後自動從中斷處繼續；小檔案一次送出
    const chunked = entry.resume !== null || entry.file.size > CHUNKED_THRESHOLD;
    if (chunked) {
      // 重新整理後把同一個檔案再拖進同一個資料夾：接著伺服器上未完成的上傳繼續
      const resume = entry.resume ?? (await this.findResumable(entry));
      // 等待查詢期間被取消了（見 cancel）
      if (entry.status !== "uploading") {
        return;
      }
      if (resume !== null) {
        entry.uploadId = resume.id;
      }
      entry.handle = startChunkedUpload(entry.file, entry.dir, {
        space: entry.space,
        onProgress,
        onNotice,
        onStarted: (id) => {
          entry.uploadId = id;
        },
        ...(resume !== null ? { resume } : {}),
      });
    } else {
      entry.handle = startUpload(entry.file, entry.dir, onProgress, entry.space);
    }

    try {
      await entry.handle.done;
      this.setProgress(entry, 1);
      this.setStatus(entry, "done", t("Done"));
      this.onUploaded(entry.dir, entry.space);
    } catch (error) {
      if (error instanceof UploadCancelled) {
        this.remove(entry);
        this.hideUntilDiscarded(entry.uploadId);
      } else {
        this.setStatus(entry, "failed", error instanceof ApiError ? error.message : t("Upload failed"));
        // 登入失效或必須修改密碼：後面的檔案也一定會失敗，全部取消
        if (isHandledGlobally(error)) {
          this.cancelPending();
        }
      }
    } finally {
      entry.handle = null;
      if (chunked) {
        this.onChunkedSettled();
      }
    }
  }

  /**
   * 伺服器上與這個檔案相符、目的地相同、而且沒有在這個分頁傳的未完成上傳。
   * 查詢失敗就當作沒有，開始一個新的上傳：續傳只是讓使用者省時間，不是必要條件
   */
  private async findResumable(entry: Entry): Promise<UploadInfo | null> {
    try {
      const uploads = await api.get<UploadInfo[]>(withSpace("/api/uploads", entry.space));
      const active = this.activeUploadIds();
      return (
        uploads.find(
          (info) =>
            info.path === entry.dir &&
            !active.has(info.id) &&
            isSameFile(entry.file, info),
        ) ?? null
      );
    } catch {
      return null;
    }
  }

  // ==================== 取消與關閉 ====================

  private cancel(entry: Entry): void {
    if (entry.status === "uploading" && entry.handle !== null) {
      // 中斷連線；結果由 uploadOne 的 catch 處理（從清單移除）
      entry.handle.cancel();
    } else if (isActive(entry)) {
      // 等待中，或還在查詢可續傳的上傳：直接移除，uploadOne 看到狀態變了就會停下
      entry.status = "failed";
      this.remove(entry);
    }
  }

  /** 取消的分段上傳：伺服器確認刪除之前不顯示在未完成的清單，刪除結束後再更新清單 */
  private hideUntilDiscarded(id: string | null): void {
    if (id === null) {
      return;
    }
    this.cancelledIds.add(id);
    void whenDiscarded(id).then(() => {
      this.cancelledIds.delete(id);
      this.onChunkedSettled();
    });
  }

  private cancelPending(): void {
    for (const entry of [...this.entries]) {
      if (entry.status === "pending") {
        this.cancel(entry);
      }
    }
  }

  /** 上傳中按 ⓧ：全部取消。只剩失敗的項目時按 ⓧ：關閉面板 */
  private closeOrCancel(): void {
    if (this.isBusy()) {
      for (const entry of [...this.entries]) {
        this.cancel(entry);
      }
      return;
    }
    this.entries = [];
    this.renderList();
  }

  private remove(entry: Entry): void {
    this.entries = this.entries.filter((other) => other !== entry);
    this.renderList();
  }

  // ==================== 畫面 ====================

  private createEntry(
    file: File,
    dir: string,
    space: Space,
    resume: UploadInfo | null,
  ): Entry {
    const fill = h("div", { class: "bar-fill" });
    const statusText = h("span", { class: "item-status" }, t("Waiting"));
    const sizeText = h("span", { class: "item-size" }, formatSize(file.size));
    const cancelButton = h(
      "button",
      {
        type: "button",
        class: "icon-button small",
        "aria-label": t("Cancel {name}", { name: file.name }),
        title: t("Cancel"),
      },
      icon("close", 16),
    );
    const row = h(
      "li",
      { class: "panel-item", "data-status": "pending" },
      h(
        "div",
        { class: "item-line" },
        h("span", { class: "item-name", title: file.name }, file.name),
        statusText,
        cancelButton,
      ),
      h("div", { class: "item-line" }, h("div", { class: "bar" }, fill), sizeText),
    );

    const entry: Entry = {
      file,
      dir,
      space,
      resume,
      status: "pending",
      handle: null,
      // 續傳的編號一開始就知道：等待中時未完成的清單就不會重複顯示它
      uploadId: resume?.id ?? null,
      fraction: 0,
      row,
      fill,
      statusText,
      sizeText,
      cancelButton,
    };
    cancelButton.addEventListener("click", () => this.cancel(entry));
    return entry;
  }

  private setStatus(entry: Entry, status: Status, text: string): void {
    entry.status = status;
    entry.statusText.textContent = text;
    entry.row.dataset.status = status;
    entry.cancelButton.hidden = !isActive(entry);
    entry.fill.classList.toggle("done", status === "done");
    entry.fill.classList.toggle("failed", status === "failed");
    if (status === "failed") {
      // 移到最上方的「Failed」一區
      this.renderList();
      return;
    }
    this.refresh();
  }

  private setProgress(entry: Entry, fraction: number): void {
    entry.fraction = Math.max(0, Math.min(1, fraction));
    entry.fill.style.width = `${(entry.fraction * 100).toFixed(1)}%`;
    entry.sizeText.textContent =
      entry.fraction >= 1
        ? formatSize(entry.file.size)
        : `${formatSize(entry.file.size * entry.fraction)} / ${formatSize(entry.file.size)}`;
    if (entry.status === "uploading") {
      entry.statusText.textContent = progressText(entry.fraction);
    }
    this.refreshSummary();
  }

  private renderList(): void {
    this.drawList();
    this.refresh();
  }

  /** 失敗的在上方一區，其他的依加入的順序 */
  private drawList(): void {
    const failed = this.entries.filter((entry) => entry.status === "failed");
    const others = this.entries.filter((entry) => entry.status !== "failed");
    const heading = (text: string): HTMLElement => h("li", { class: "panel-section", role: "presentation" }, text);
    this.list.replaceChildren(
      ...(failed.length > 0 ? [heading(t("Failed")), ...failed.map((entry) => entry.row)] : []),
      ...(failed.length > 0 && others.length > 0 ? [heading(t("Other uploads"))] : []),
      ...others.map((entry) => entry.row),
    );
  }

  /** 依目前的狀態更新標題、按鈕與總進度，決定面板是否顯示 */
  private refresh(): void {
    window.clearTimeout(this.hideTimer);
    this.element.hidden = this.entries.length === 0;
    if (this.entries.length === 0) {
      this.stopTicker();
      return;
    }

    const active = this.isBusy();
    const failed = this.entries.filter((entry) => entry.status === "failed").length;
    this.element.classList.toggle("failed", !active && failed > 0);
    this.summary.hidden = !active;

    if (active) {
      this.title.textContent = t("Uploading…");
      this.setButton(t("Cancel all uploads"));
      this.startTicker();
    } else if (failed > 0) {
      // 全部結束：只留下失敗的項目
      if (this.entries.some((entry) => entry.status !== "failed")) {
        this.entries = this.entries.filter((entry) => entry.status === "failed");
        this.drawList();
      }
      this.title.textContent = failed === 1 ? t("1 upload failed") : t("{count} uploads failed", { count: failed });
      this.count.textContent = "";
      this.setButton(t("Close"));
      this.stopTicker();
    } else {
      this.title.textContent = t("Uploads complete");
      this.count.textContent = "";
      this.closeButton.hidden = true;
      this.stopTicker();
      // 全部成功：讓使用者看一眼，然後自動消失
      this.hideTimer = window.setTimeout(() => {
        this.entries = [];
        this.renderList();
      }, HIDE_AFTER_MS);
    }
    this.refreshSummary();
  }

  private setButton(label: string): void {
    this.closeButton.hidden = false;
    this.closeButton.setAttribute("aria-label", label);
    this.closeButton.title = label;
  }

  /** 總進度：這一批所有檔案（失敗的除外）已傳的位元組 */
  private refreshSummary(): void {
    if (!this.isBusy()) {
      return;
    }
    const { done, total } = this.totals();
    const fraction = total > 0 ? done / total : 0;
    this.count.textContent = `(${Math.floor(fraction * 100)}%)`;
    this.summaryFill.style.width = `${(fraction * 100).toFixed(1)}%`;
    this.statBytes.textContent = `${formatSize(done)} / ${formatSize(total)}`;
  }

  private totals(): { done: number; total: number } {
    let done = 0;
    let total = 0;
    for (const entry of this.entries) {
      if (entry.status !== "failed") {
        total += entry.file.size;
        done += entry.file.size * entry.fraction;
      }
    }
    return { done, total };
  }

  // ---------- 速度與剩餘時間 ----------

  /** 上傳中每秒量測一次速度，用移動平均讓數字不要跳動得太厲害 */
  private startTicker(): void {
    if (this.ticker !== undefined) {
      return;
    }
    this.lastSample = null;
    this.speed = 0;
    this.ticker = window.setInterval(() => this.sample(), 1000);
  }

  private stopTicker(): void {
    window.clearInterval(this.ticker);
    this.ticker = undefined;
    this.lastSample = null;
    this.speed = 0;
    this.statSpeed.textContent = "";
    this.statLeft.textContent = "";
  }

  private sample(): void {
    const now = performance.now();
    const { done, total } = this.totals();
    if (this.lastSample !== null) {
      const seconds = (now - this.lastSample.time) / 1000;
      const current = seconds > 0 ? Math.max(0, done - this.lastSample.bytes) / seconds : 0;
      this.speed =
        this.speed === 0 ? current : this.speed + SPEED_SMOOTHING * (current - this.speed);
    }
    this.lastSample = { time: now, bytes: done };

    if (this.speed > 0) {
      this.statSpeed.textContent = `${formatSize(this.speed)}/s`;
      this.statLeft.textContent = t("About {time} left", { time: formatDuration((total - done) / this.speed) });
    } else {
      this.statSpeed.textContent = "";
      this.statLeft.textContent = "";
    }
  }

  // ---------- 縮小 ----------

  private setMinimized(minimized: boolean): void {
    this.minimized = minimized;
    saveFlag(MINIMIZED_KEY, minimized);
    this.applyMinimized();
  }

  private applyMinimized(): void {
    this.body.hidden = this.minimized;
    const label = this.minimized ? t("Show uploads") : t("Minimize uploads");
    this.toggleButton.setAttribute("aria-expanded", String(!this.minimized));
    this.toggleButton.setAttribute("aria-label", label);
    this.toggleButton.title = label;
  }
}

function isActive(entry: Entry): boolean {
  return entry.status === "pending" || entry.status === "uploading";
}

function progressText(fraction: number): string {
  return t("Uploading {percent}%", { percent: Math.floor(fraction * 100) });
}

/** 讀取記在瀏覽器中的開關；無法使用 localStorage 時視為關閉 */
export function loadFlag(key: string): boolean {
  try {
    return localStorage.getItem(key) === "1";
  } catch {
    return false;
  }
}

export function saveFlag(key: string, value: boolean): void {
  try {
    localStorage.setItem(key, value ? "1" : "0");
  } catch {
    // 無法儲存只代表下次回到預設
  }
}
