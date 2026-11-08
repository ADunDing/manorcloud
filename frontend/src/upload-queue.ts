// 上傳佇列：依序上傳多個檔案，顯示每個檔案的進度與狀態。
//
// 佇列是獨立存在的物件，自己持有一個 DOM 元素。檔案列表每次重新載入都會換掉整個畫面，
// 但只要把同一個元素再放回去，進度與狀態就會保留（DOM 元素被重新 append 時是搬移，不是複製）。

import { api, ApiError, isHandledGlobally } from "./api.js";
import {
  CHUNKED_THRESHOLD,
  isSameFile,
  startChunkedUpload,
  type UploadInfo,
} from "./chunked-upload.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { startUpload, UploadCancelled, type UploadHandle } from "./upload.js";

type Status = "pending" | "uploading" | "done" | "failed" | "cancelled";

/** 佇列中的一個檔案。畫面上的元素也存在這裡，更新進度時只改這一列，不重繪整個佇列 */
interface Entry {
  readonly file: File;
  /** 上傳的目標資料夾：使用者中途切換到別的資料夾，檔案仍然傳到原本的地方 */
  readonly folderId: number | null;
  /** 要續傳的伺服器上的上傳（從未完成的清單按「繼續」時） */
  readonly resume: UploadInfo | null;
  status: Status;
  handle: UploadHandle | null;
  /** 分段上傳的編號：未完成的清單用它排除這個分頁正在傳的上傳 */
  uploadId: string | null;
  readonly row: HTMLElement;
  readonly bar: HTMLProgressElement;
  readonly statusText: HTMLElement;
  readonly cancelButton: HTMLButtonElement;
}

export class UploadQueue {
  /** 佇列的畫面，由檔案瀏覽畫面每次重繪時放回去 */
  readonly element: HTMLElement;
  private readonly list: HTMLElement;
  private entries: Entry[] = [];
  private running = false;

  /**
   * @param onUploaded 一個檔案上傳完成時呼叫，傳入它的目標資料夾（用來決定要不要重新載入列表）
   * @param uploadLimit 取得上傳上限；無法取得時回傳 null，此時不在前端預先檢查
   * @param onChunkedSettled 一個分段上傳結束時呼叫（完成、失敗或取消）：未完成的清單可能變了
   */
  constructor(
    private readonly onUploaded: (folderId: number | null) => void,
    private readonly uploadLimit: () => Promise<number | null>,
    private readonly onChunkedSettled: () => void,
  ) {
    this.list = h("ul", { class: "upload-list" });
    this.element = h(
      "section",
      { class: "upload-queue", "aria-label": "上傳" },
      h(
        "div",
        { class: "upload-header" },
        h("strong", {}, "上傳"),
        h(
          "button",
          {
            type: "button",
            class: "link",
            onClick: () => this.clearFinished(),
          },
          "清除已完成",
        ),
      ),
      this.list,
    );
    this.element.hidden = true;
  }

  /** 加入檔案並開始依序上傳 */
  add(files: readonly File[], folderId: number | null): void {
    for (const file of files) {
      this.entries.push(this.createEntry(file, folderId, null));
    }
    this.renderList();
    void this.run();
  }

  /** 續傳伺服器上未完成的上傳：傳回原本的資料夾，不是目前所在的資料夾 */
  resume(file: File, info: UploadInfo): void {
    this.entries.push(this.createEntry(file, info.parent_id, info));
    this.renderList();
    void this.run();
  }

  /** 是否還有檔案正在上傳或等待上傳 */
  isBusy(): boolean {
    return this.entries.some(
      (entry) => entry.status === "pending" || entry.status === "uploading",
    );
  }

  /** 這個分頁正在傳、或等待續傳的分段上傳編號 */
  activeUploadIds(): Set<string> {
    const ids = new Set<string>();
    for (const entry of this.entries) {
      const active = entry.status === "pending" || entry.status === "uploading";
      if (active && entry.uploadId !== null) {
        ids.add(entry.uploadId);
      }
    }
    return ids;
  }

  // ---------- 執行 ----------

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
      this.setStatus(entry, "failed", `超過大小上限（${formatSize(limit)}）`);
      return;
    }

    this.setStatus(entry, "uploading", "上傳中 0%");
    const onProgress = (fraction: number): void => {
      entry.bar.value = fraction;
      entry.statusText.textContent = `上傳中 ${Math.floor(fraction * 100)}%`;
    };
    // 重試等提示暫時取代進度文字；清除時恢復顯示目前的百分比
    const onNotice = (message: string | null): void => {
      entry.statusText.textContent =
        message ?? `上傳中 ${Math.floor(entry.bar.value * 100)}%`;
    };

    // 大檔案分段上傳：斷線後自動從中斷處繼續；小檔案一次送出
    const chunked =
      entry.resume !== null || entry.file.size > CHUNKED_THRESHOLD;
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
      entry.handle = startChunkedUpload(entry.file, entry.folderId, {
        onProgress,
        onNotice,
        onStarted: (id) => {
          entry.uploadId = id;
        },
        ...(resume !== null ? { resume } : {}),
      });
    } else {
      entry.handle = startUpload(entry.file, entry.folderId, onProgress);
    }

    try {
      await entry.handle.done;
      entry.bar.value = 1;
      this.setStatus(entry, "done", "完成");
      this.onUploaded(entry.folderId);
    } catch (error) {
      if (error instanceof UploadCancelled) {
        this.setStatus(entry, "cancelled", "已取消");
      } else {
        this.setStatus(
          entry,
          "failed",
          error instanceof ApiError ? error.message : "上傳失敗",
        );
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
      const uploads = await api.get<UploadInfo[]>("/api/uploads");
      const active = this.activeUploadIds();
      return (
        uploads.find(
          (info) =>
            info.parent_id === entry.folderId &&
            !active.has(info.id) &&
            isSameFile(entry.file, info),
        ) ?? null
      );
    } catch {
      return null;
    }
  }

  private cancel(entry: Entry): void {
    if (entry.status === "uploading") {
      if (entry.handle !== null) {
        // 中斷連線；結果由 uploadOne 的 catch 處理
        entry.handle.cancel();
      } else {
        // 還在查詢可續傳的上傳，沒有可以中斷的請求：直接標記，uploadOne 查詢結束後會停下
        this.setStatus(entry, "cancelled", "已取消");
      }
    } else if (entry.status === "pending") {
      this.setStatus(entry, "cancelled", "已取消");
    }
  }

  private cancelPending(): void {
    for (const entry of this.entries) {
      if (entry.status === "pending") {
        this.setStatus(entry, "cancelled", "已取消");
      }
    }
  }

  private clearFinished(): void {
    this.entries = this.entries.filter(
      (entry) => entry.status === "pending" || entry.status === "uploading",
    );
    this.renderList();
  }

  // ---------- 畫面 ----------

  private createEntry(
    file: File,
    folderId: number | null,
    resume: UploadInfo | null,
  ): Entry {
    const bar = h("progress", { max: 1, value: 0 });
    const statusText = h("span", { class: "upload-status" }, "等待中");
    const cancelButton = h("button", { type: "button", class: "link" }, "取消");
    const row = h(
      "li",
      {},
      h("span", { class: "upload-name" }, file.name),
      h("span", { class: "muted" }, formatSize(file.size)),
      bar,
      statusText,
      cancelButton,
    );

    const entry: Entry = {
      file,
      folderId,
      resume,
      status: "pending",
      handle: null,
      // 續傳的編號一開始就知道：等待中時清單就不會重複顯示它
      uploadId: resume?.id ?? null,
      row,
      bar,
      statusText,
      cancelButton,
    };
    cancelButton.addEventListener("click", () => this.cancel(entry));
    return entry;
  }

  private setStatus(entry: Entry, status: Status, text: string): void {
    entry.status = status;
    entry.statusText.textContent = text;
    entry.row.dataset.status = status;
    // 只有等待中與上傳中可以取消
    entry.cancelButton.hidden = status !== "pending" && status !== "uploading";
  }

  private renderList(): void {
    this.list.replaceChildren(...this.entries.map((entry) => entry.row));
    this.element.hidden = this.entries.length === 0;
  }
}
