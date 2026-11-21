// 跨硬碟的複製與移動：伺服器在背景進行，右下角的面板顯示進度。
// 進行中的可以取消。順利完成或取消的幾秒後自動從清單移除；
// 失敗或有略過項目的會留著顯示原因，按 × 移除。
// 有進行中的項目時每兩秒更新一次；全部結束後停止更新。

import { t } from "./i18n.js";
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
      { class: "panel", "aria-label": t("Copying and moving"), hidden: true },
      h(
        "div",
        { class: "panel-header" },
        h("div", { class: "panel-heading" }, h("strong", { class: "panel-title" }, t("Copy and move"))),
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
      const running = new Set(items.filter((job) => job.state === "running").map((job) => job.id));
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
    for (const job of items) {
      const quiet = (job.state === "done" || job.state === "cancelled") && job.skipped.length === 0;
      if (!quiet || this.dismissing.has(job.id)) {
        continue;
      }
      this.dismissing.add(job.id);
      window.setTimeout(() => {
        void api
          .delete(`/api/transfers/${job.id}`)
          .catch(() => undefined)
          .finally(() => this.refresh());
      }, DISMISS_MS);
    }
  }

  private render(items: readonly Transfer[]): void {
    this.element.hidden = items.length === 0;
    this.list.replaceChildren(...items.map((job) => this.row(job)));
  }

  private row(job: Transfer): HTMLElement {
    const target = [this.labelOf(job.to_space), ...job.to_path.split("/").filter((p) => p !== "")].join(" / ");
    const one = job.names.length === 1;
    const params = { name: job.names[0] ?? "", count: job.names.length, target };
    const move = job.kind === "move";
    const heading = {
      running: move
        ? one ? t("Moving \"{name}\" to {target}", params) : t("Moving {count} items to {target}", params)
        : one ? t("Copying \"{name}\" to {target}", params) : t("Copying {count} items to {target}", params),
      done: move
        ? one ? t("Moved \"{name}\" to {target}", params) : t("Moved {count} items to {target}", params)
        : one ? t("Copied \"{name}\" to {target}", params) : t("Copied {count} items to {target}", params),
      failed: move
        ? one ? t("Couldn't move \"{name}\" to {target}", params) : t("Couldn't move {count} items to {target}", params)
        : one ? t("Couldn't copy \"{name}\" to {target}", params) : t("Couldn't copy {count} items to {target}", params),
      cancelled: one
        ? t("Cancelled: \"{name}\" to {target}", params)
        : t("Cancelled: {count} items to {target}", params),
    }[job.state];
    const fraction = job.total_bytes > 0 ? Math.min(1, job.done_bytes / job.total_bytes) : job.state === "done" ? 1 : 0;
    const fill = h("div", {
      class: `bar-fill${job.state === "done" ? " done" : job.state === "failed" ? " failed" : ""}`,
    });
    fill.style.width = `${(fraction * 100).toFixed(1)}%`;
    const message = h("span", { class: "pending-message", role: "status" });

    const detail =
      job.state === "running"
        ? t("{done} of {total}", { done: formatSize(job.done_bytes), total: formatSize(job.total_bytes) })
        : job.state === "failed"
          ? (job.error ?? t("Something went wrong"))
          : job.state === "cancelled"
            ? t("{done} of {total} finished before cancelling", { done: job.done_items, total: job.total_items })
            : `${formatSize(job.total_bytes)}`;

    return h(
      "li",
      { class: "panel-item pending-item" },
      h(
        "div",
        { class: "pending-text" },
        h("span", { class: "item-name", title: heading }, heading),
        h("span", { class: "pending-progress" }, detail),
        ...(job.state === "running" || job.state === "done" ? [h("div", { class: "bar" }, fill)] : []),
        ...(job.skipped.length > 0
          ? [
              h(
                "details",
                { class: "pending-progress" },
                h("summary", {}, t("{count} skipped", { count: job.skipped.length })),
                h("ul", {}, job.skipped.map((s) => h("li", {}, s))),
              ),
            ]
          : []),
      ),
      job.state === "running"
        ? h("button", { type: "button", class: "link", onClick: () => void this.remove(job, message) }, t("Cancel"))
        : h(
            "button",
            {
              type: "button",
              class: "round-button",
              "aria-label": t("Remove from the list"),
              title: t("Remove from the list"),
              onClick: () => void this.remove(job, message),
            },
            icon("close"),
          ),
      message,
    );
  }

  /** 進行中：取消；已經結束：從清單移除 */
  private async remove(job: Transfer, message: HTMLElement): Promise<void> {
    try {
      await api.delete(`/api/transfers/${job.id}`);
    } catch (error) {
      if (!isHandledGlobally(error)) {
        message.textContent = error instanceof ApiError ? error.message : t("Something went wrong");
      }
      return;
    }
    this.refresh();
  }
}
