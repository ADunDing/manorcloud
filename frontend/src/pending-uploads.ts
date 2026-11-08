// 未完成的上傳：伺服器上還留著、但這個分頁沒有在傳的分段上傳。
// 通常來自重新整理、關閉分頁、或重試多次仍然斷線。
//
// 瀏覽器重新整理後不會記得使用者選過哪個檔案，所以「繼續」需要使用者再選一次同一個檔案，
// 比對相符（見 isSameFile）才從中斷處繼續。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { isSameFile, type UploadInfo } from "./chunked-upload.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import type { UploadQueue } from "./upload-queue.js";

export class PendingUploads {
  /** 清單的畫面，由檔案瀏覽畫面每次重繪時放回去 */
  readonly element: HTMLElement;
  private readonly list: HTMLElement;
  /** 載入序號：較早送出、較晚回來的回應不能蓋掉較新的結果 */
  private loading = 0;

  constructor(private readonly queue: UploadQueue) {
    this.list = h("ul", { class: "pending-list" });
    this.element = h(
      "section",
      { class: "pending-uploads", "aria-label": "未完成的上傳" },
      h("strong", {}, "未完成的上傳"),
      h(
        "p",
        { class: "muted" },
        "選擇同一個檔案即可從中斷處繼續。24 小時沒有進度的上傳會自動刪除。",
      ),
      this.list,
    );
    this.element.hidden = true;
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

    // 這個分頁正在傳的不顯示：否則它會同時出現在佇列與清單中，讓人以為它中斷了
    const active = this.queue.activeUploadIds();
    const rows = uploads
      .filter((info) => !active.has(info.id))
      .map((info) => this.row(info));
    this.list.replaceChildren(...rows);
    this.element.hidden = rows.length === 0;
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
        message.textContent = "這不是同一個檔案：名稱、大小或修改時間不符";
        return;
      }
      this.queue.resume(file, info);
      this.refresh();
    });

    const percent = info.size > 0 ? Math.floor((info.received / info.size) * 100) : 0;

    return h(
      "li",
      {},
      h("span", { class: "upload-name" }, info.name),
      h(
        "span",
        { class: "muted" },
        `已傳 ${formatSize(info.received)} / ${formatSize(info.size)}（${percent}%）`,
      ),
      h("button", { type: "button", onClick: () => picker.click() }, "繼續"),
      h(
        "button",
        { type: "button", class: "link", onClick: () => this.cancel(info, message) },
        "取消",
      ),
      message,
      picker,
    );
  }

  private cancel(info: UploadInfo, message: HTMLElement): void {
    // confirm 的文字是純文字，檔名不會被當成 HTML 解析
    if (!window.confirm(`確定要取消「${info.name}」的上傳嗎？已傳的部分會被刪除。`)) {
      return;
    }
    void (async () => {
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
        message.textContent = error instanceof ApiError ? error.message : "取消失敗";
      }
    })();
  }
}
