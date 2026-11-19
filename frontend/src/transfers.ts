// 跨硬碟的複製與移動：伺服器在背景進行，右下角的面板顯示進度。
// 進行中的可以取消。順利完成或取消的幾秒後自動從清單移除；
// 失敗或有略過項目的會留著顯示原因，按 × 移除。
// 有進行中的項目時每兩秒更新一次；全部結束後停止更新。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { icon } from "./icons.js";

/** GET /api/transfers 的一筆。對應 Rust 端 web/transfers.rs 的 TransferResponse */
export interface Transfer {
  readonly id: number;
  readonly kind: "copy" | "move";
  readonly names: readonly string[];
  readonly from_space: string;
  readonly to_space: string;
  readonly to_path: string;
  readonly total_items: number;
  readonly done_items: number;
  readonly total_bytes: number;
  readonly done_bytes: number;
  readonly state: "running" | "done" | "failed" | "cancelled";
  readonly error: string | null;
  readonly skipped: readonly string[];
}

const POLL_MS = 2000;
/** 順利完成的項目顯示多久後自動移除 */
const DISMISS_MS = 4000;

export class TransferPanel {
  /** 面板，由檔案瀏覽畫面放進右下角 */
  readonly element: HTMLElement;
  private readonly list = h("ul", { class: "panel-list" });
  private timer: number | null = null;
  /** 上一次看到的進行中項目：變成結束時通知畫面重新載入 */
  private running = new Set<number>();
  private loading = 0;
  /** 已經排定自動移除的項目 */
  private readonly dismissing = new Set<number>();

  /**
   * @param labelOf  space= 的值 → 顯示的名稱（My files、Disk2）
   * @param onFinished 有項目結束時呼叫（重新載入目前的資料夾）
   */
  constructor(
    private readonly labelOf: (space: string) => string,
    private readonly onFinished: () => void,
  ) {
    this.element = h(
      "section",
      { class: "panel", "aria-label": "Copying and moving", hidden: true },
      h(
        "div",
        { class: "panel-header" },
        h("div", { class: "panel-heading" }, h("strong", { class: "panel-title" }, "Copy and move")),
      ),
      h("div", { class: "panel-body" }, this.list),
    );
  }

  /** 重新讀取清單 */
  refresh(): void {
    if (this.timer !== null) {
      window.clearTimeout(this.timer);
      this.timer = null;
    }
    const token = ++this.loading;
    void (async () => {
      let items: Transfer[];
      try {
        items = await api.get<Transfer[]>("/api/transfers");
      } catch {
        // 只是進度顯示：下次再試
        this.schedule();
        return;
      }
      if (token !== this.loading) {
        return;
      }
      const running = new Set(items.filter((t) => t.state === "running").map((t) => t.id));
      const finished = [...this.running].some((id) => !running.has(id));
      this.running = running;
      this.render(items);
      this.scheduleDismiss(items);
      if (finished) {
        this.onFinished();
      }
      if (running.size > 0) {
        this.schedule();
      }
    })();
  }

  private schedule(): void {
    this.timer = window.setTimeout(() => this.refresh(), POLL_MS);
  }

  /** 順利完成、取消的項目過幾秒自動移除；失敗或有略過的留著讓使用者看 */
  private scheduleDismiss(items: readonly Transfer[]): void {
    for (const t of items) {
      const quiet = (t.state === "done" || t.state === "cancelled") && t.skipped.length === 0;
      if (!quiet || this.dismissing.has(t.id)) {
        continue;
      }
      this.dismissing.add(t.id);
      window.setTimeout(() => {
        void api
          .delete(`/api/transfers/${t.id}`)
          .catch(() => undefined)
          .finally(() => this.refresh());
      }, DISMISS_MS);
    }
  }

  private render(items: readonly Transfer[]): void {
    this.element.hidden = items.length === 0;
    this.list.replaceChildren(...items.map((t) => this.row(t)));
  }

  private row(t: Transfer): HTMLElement {
    const what = t.names.length === 1 ? `"${t.names[0] ?? ""}"` : `${t.names.length} items`;
    const target = [this.labelOf(t.to_space), ...t.to_path.split("/").filter((p) => p !== "")].join(" / ");
    const verb = {
      running: t.kind === "move" ? "Moving" : "Copying",
      done: t.kind === "move" ? "Moved" : "Copied",
      failed: t.kind === "move" ? "Couldn't move" : "Couldn't copy",
      cancelled: "Cancelled",
    }[t.state];
    const fraction = t.total_bytes > 0 ? Math.min(1, t.done_bytes / t.total_bytes) : t.state === "done" ? 1 : 0;
    const fill = h("div", {
      class: `bar-fill${t.state === "done" ? " done" : t.state === "failed" ? " failed" : ""}`,
    });
    fill.style.width = `${(fraction * 100).toFixed(1)}%`;
    const message = h("span", { class: "pending-message", role: "status" });

    const detail =
      t.state === "running"
        ? `${formatSize(t.done_bytes)} of ${formatSize(t.total_bytes)}`
        : t.state === "failed"
          ? (t.error ?? "Something went wrong")
          : t.state === "cancelled"
            ? `${t.done_items} of ${t.total_items} finished before cancelling`
            : `${formatSize(t.total_bytes)}`;

    return h(
      "li",
      { class: "panel-item pending-item" },
      h(
        "div",
        { class: "pending-text" },
        h("span", { class: "item-name", title: `${verb} ${what} to ${target}` }, `${verb} ${what} to ${target}`),
        h("span", { class: "pending-progress" }, detail),
        ...(t.state === "running" || t.state === "done" ? [h("div", { class: "bar" }, fill)] : []),
        ...(t.skipped.length > 0
          ? [
              h(
                "details",
                { class: "pending-progress" },
                h("summary", {}, `${t.skipped.length} skipped`),
                h("ul", {}, t.skipped.map((s) => h("li", {}, s))),
              ),
            ]
          : []),
      ),
      t.state === "running"
        ? h("button", { type: "button", class: "link", onClick: () => void this.remove(t, message) }, "Cancel")
        : h(
            "button",
            {
              type: "button",
              class: "round-button",
              "aria-label": "Remove from the list",
              title: "Remove from the list",
              onClick: () => void this.remove(t, message),
            },
            icon("close"),
          ),
      message,
    );
  }

  /** 進行中：取消；已經結束：從清單移除 */
  private async remove(t: Transfer, message: HTMLElement): Promise<void> {
    try {
      await api.delete(`/api/transfers/${t.id}`);
    } catch (error) {
      if (!isHandledGlobally(error)) {
        message.textContent = error instanceof ApiError ? error.message : "Something went wrong";
      }
      return;
    }
    this.refresh();
  }
}
