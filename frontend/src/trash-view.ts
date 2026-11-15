// 垃圾桶：列出刪除的項目，可以還原、永久刪除或清空。
//
// 列表與 My files 共用 FileList：多選、框選、右鍵選單、排序、List／Grid 的行為完全相同，
// 只是不能拖曳，並多了一欄原本的位置。
// 只列出使用者刪除的「那一個」項目；資料夾裡面的內容隨著資料夾一起還原或刪除。
// 超過 30 天的項目由伺服器的垃圾回收永久刪除。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { confirmDialog } from "./dialogs.js";
import { h } from "./dom.js";
import { FileList, loadView, saveView, type ViewMode } from "./file-list.js";
import { icon, type IconName } from "./icons.js";
import { openContextMenu } from "./menu.js";
import { refreshStorage } from "./sidebar.js";
import {
  defaultDirection,
  loadSort,
  saveSort,
  sortItems,
  type SortKey,
  type SortOptions,
} from "./sort.js";
import { sortControl, type SortControl } from "./sort-menu.js";
import type { FileItem, TrashItem } from "./types.js";

/** 垃圾桶的排序與 My files 分開記住；預設以刪除時間由新到舊、資料夾與檔案混合 */
const SORT_KEY = "manorcloud.trash-sort";
const DEFAULT_TRASH_SORT: SortOptions = { key: "modified", direction: "desc", foldersFirst: false };

/** 載入序號：離開垃圾桶或重新載入後，較慢回來的舊結果不能蓋掉新的畫面 */
let loading = 0;

/** 目前顯示的垃圾桶畫面；不在垃圾桶時為 null */
let current: TrashView | null = null;

/** 在 container 中顯示垃圾桶。notice：重新載入後要顯示的錯誤 */
export function renderTrash(container: HTMLElement, notice = ""): void {
  current?.unmount();
  current = null;
  const token = ++loading;
  container.replaceChildren(h("p", { class: "muted" }, "Loading…"));

  void (async () => {
    try {
      const items = await api.get<TrashItem[]>("/api/trash");
      if (token !== loading) {
        return;
      }
      current = new TrashView(container, items, notice);
      current.mount();
    } catch (error) {
      if (token !== loading || isHandledGlobally(error)) {
        return;
      }
      container.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
    }
  })();
}

/** 離開垃圾桶時呼叫：移除鍵盤的監聽，還在路上的回應不再更新畫面 */
export function leaveTrash(): void {
  loading += 1;
  current?.unmount();
  current = null;
}

class TrashView {
  private readonly items: FileItem[];
  /** 項目編號 → 原本的位置（My files / 相片 / 2026） */
  private readonly locations = new Map<number, string>();
  private readonly errorBox: HTMLElement;
  private readonly controller = new AbortController();
  private readonly sorter: SortControl;
  private list: FileList;
  private view: ViewMode = loadView();
  private sort: SortOptions = loadSort(SORT_KEY, DEFAULT_TRASH_SORT);
  private busy = false;

  private readonly buttons: {
    restore: HTMLButtonElement;
    remove: HTMLButtonElement;
    empty: HTMLButtonElement;
  };
  private readonly viewButtons: { list: HTMLButtonElement; grid: HTMLButtonElement };

  constructor(
    private readonly container: HTMLElement,
    trash: readonly TrashItem[],
    notice: string,
  ) {
    // 轉成列表用的格式：時間那一欄顯示刪除的時間
    this.items = trash.map((item) => {
      this.locations.set(item.id, ["My files", ...item.location].join(" / "));
      return {
        id: item.id,
        parent_id: null,
        name: item.name,
        kind: item.kind,
        size: item.size,
        mime: null,
        created_at: item.trashed_at,
        updated_at: item.trashed_at,
      };
    });
    this.errorBox = h("p", { class: "error", role: "alert" }, notice);

    this.buttons = {
      restore: toolButton("Restore", "restore", () => void this.restoreSelected()),
      remove: toolButton("Delete forever", "trash", () => void this.deleteSelected()),
      // 放在標題旁邊，與右邊的 Delete forever 分開，避免誤觸。垃圾桶是空的時候不顯示
      empty: h(
        "button",
        {
          type: "button",
          class: "title-action danger",
          hidden: this.items.length === 0,
          onClick: () => void this.emptyTrash(),
        },
        icon("trash", 16),
        "Empty trash",
      ),
    };
    this.buttons.remove.classList.add("danger");
    this.viewButtons = {
      list: viewButton("List view", "list", () => this.setView("list")),
      grid: viewButton("Grid view", "grid", () => this.setView("grid")),
    };

    this.sorter = sortControl(this.sort, (sort) => this.setSort(sort), "Deleted");
    this.list = this.createList();
  }

  mount(): void {
    this.container.replaceChildren(
      h(
        "div",
        { class: "folder-header" },
        // 左邊：標題與 Empty trash
        h(
          "div",
          { class: "trash-title" },
          h(
            "nav",
            { class: "breadcrumbs", "aria-label": "Current location" },
            h("ol", {}, h("li", {}, h("span", { "aria-current": "page" }, "Trash"))),
          ),
          this.buttons.empty,
        ),
        // 右邊：與 My files 相同的位置
        h(
          "div",
          { class: "toolbar", role: "toolbar", "aria-label": "Trash actions" },
          this.buttons.restore,
          this.buttons.remove,
        ),
        h(
          "div",
          { class: "view-switch", role: "group", "aria-label": "View" },
          this.viewButtons.list,
          this.viewButtons.grid,
        ),
      ),
      h(
        "div",
        { class: "trash-banner" },
        icon("info", 18),
        h("span", {}, "Items in the trash are deleted forever after 30 days."),
      ),
      this.errorBox,
      this.list.element,
    );
    this.updateViewButtons();
    this.updateToolbar();
    this.bindKeys();
    // 框選可以從內容區的任何空白處開始，與 My files 相同
    this.container.addEventListener("mousedown", (event) => this.list.startMarquee(event), {
      signal: this.controller.signal,
    });
  }

  unmount(): void {
    this.controller.abort();
  }

  // ---------- 列表、檢視與排序 ----------

  private createList(): FileList {
    return new FileList(sortItems(this.items, this.sort), {
      view: this.view,
      // 垃圾桶中的項目不能開啟：資料夾的內容要還原之後才看得到
      onOpen: () => undefined,
      onSelectionChange: () => this.updateToolbar(),
      onContextMenu: (x, y) => this.showContextMenu(x, y),
      // 垃圾桶中不能改名（F2 只在 My files 有作用）
      onRename: async () => undefined,
      marqueeHost: this.container,
      sort: this.sort,
      onSort: (key) => this.sortBy(key),
      sortControl: this.sorter.element,
      draggable: false,
      location: (item) => this.locations.get(item.id) ?? "My files",
      timeLabel: "Deleted",
      emptyMessage: "The trash is empty.",
    });
  }

  private setView(view: ViewMode): void {
    if (view === this.view) {
      return;
    }
    this.view = view;
    saveView(view);
    this.rebuildList();
    this.updateViewButtons();
  }

  private sortBy(key: SortKey): void {
    const { sort } = this;
    this.setSort(
      sort.key === key
        ? { ...sort, direction: sort.direction === "asc" ? "desc" : "asc" }
        : { ...sort, key, direction: defaultDirection(key) },
    );
  }

  private setSort(sort: SortOptions): void {
    this.sort = sort;
    saveSort(sort, SORT_KEY);
    this.rebuildList();
    this.sorter.update(sort);
  }

  private rebuildList(): void {
    const selected = this.list.selectedItems().map((item) => item.id);
    const next = this.createList();
    this.list.element.replaceWith(next.element);
    this.list = next;
    this.list.select(selected);
  }

  private updateViewButtons(): void {
    this.viewButtons.list.setAttribute("aria-pressed", String(this.view === "list"));
    this.viewButtons.grid.setAttribute("aria-pressed", String(this.view === "grid"));
  }

  private updateToolbar(): void {
    const none = this.list.selectedItems().length === 0;
    this.buttons.restore.disabled = none;
    this.buttons.remove.disabled = none;
  }

  private showContextMenu(x: number, y: number): void {
    const selected = this.list.selectedItems();
    const title = selected.length === 1 ? (selected[0]?.name ?? "") : `${selected.length} items selected`;
    openContextMenu(x, y, title, [
      { label: "Restore", icon: "restore", onSelect: () => void this.restoreSelected() },
      "separator",
      {
        label: "Delete forever",
        icon: "trash",
        danger: true,
        onSelect: () => void this.deleteSelected(),
      },
    ]);
  }

  // ---------- 操作 ----------

  private async restoreSelected(): Promise<void> {
    await this.each(this.list.selectedItems(), "Restored", false, (item) =>
      api.post(`/api/trash/${item.id}/restore`, {}),
    );
  }

  private async deleteSelected(): Promise<void> {
    const selected = this.list.selectedItems();
    const [first] = selected;
    if (first === undefined) {
      return;
    }
    const message =
      selected.length > 1
        ? `${selected.length} items will be deleted forever. Folders are deleted with everything in them. This can't be undone.`
        : first.kind === "folder"
          ? `"${first.name}" and everything in it will be deleted forever. This can't be undone.`
          : `"${first.name}" will be deleted forever. This can't be undone.`;
    const confirmed = await confirmDialog({
      title: "Delete forever?",
      message,
      confirmLabel: "Delete forever",
      danger: true,
    });
    if (confirmed) {
      await this.each(selected, "Deleted", true, (item) => api.delete(`/api/trash/${item.id}`));
    }
  }

  private async emptyTrash(): Promise<void> {
    const confirmed = await confirmDialog({
      title: "Empty trash?",
      message: "Everything in the trash will be deleted forever. This can't be undone.",
      confirmLabel: "Empty trash",
      danger: true,
    });
    if (!confirmed || this.busy) {
      return;
    }
    this.busy = true;
    let message = "";
    try {
      await api.delete("/api/trash");
    } catch (error) {
      if (isHandledGlobally(error)) {
        return;
      }
      message = messageOf(error);
    }
    refreshStorage();
    renderTrash(this.container, message);
  }

  /**
   * 對每個項目依序執行一個操作，結束後重新載入。
   * 有失敗時不中斷其他項目，最後說明成功了幾個以及第一個錯誤的原因
   */
  private async each(
    items: readonly FileItem[],
    verb: string,
    changesStorage: boolean,
    action: (item: FileItem) => Promise<unknown>,
  ): Promise<void> {
    if (this.busy || items.length === 0) {
      return;
    }
    this.busy = true;
    let succeeded = 0;
    let firstError: unknown = null;
    for (const item of items) {
      try {
        await action(item);
        succeeded += 1;
      } catch (error) {
        if (isHandledGlobally(error)) {
          return;
        }
        firstError ??= error;
      }
    }

    let message = "";
    if (firstError !== null) {
      message =
        items.length > 1
          ? `${verb} ${succeeded} of ${items.length} items. ${messageOf(firstError)}`
          : messageOf(firstError);
    }
    if (changesStorage) {
      refreshStorage();
    }
    renderTrash(this.container, message);
  }

  // ---------- 鍵盤 ----------

  /** Ctrl／⌘＋A 全選、Esc 取消選取，與 My files 相同 */
  private bindKeys(): void {
    window.addEventListener(
      "keydown",
      (event) => {
        if (event.defaultPrevented || isTyping(event.target) || dialogOpen()) {
          return;
        }
        if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "a") {
          event.preventDefault();
          this.list.selectAll();
        } else if (event.key === "Escape") {
          this.list.clear();
        }
      },
      { signal: this.controller.signal },
    );
  }
}

// ==================== 輔助函式 ====================

function toolButton(label: string, name: IconName, onClick: () => void): HTMLButtonElement {
  return h(
    "button",
    { type: "button", class: "icon-button", "aria-label": label, title: label, onClick },
    icon(name),
  );
}

function viewButton(label: string, name: IconName, onClick: () => void): HTMLButtonElement {
  return h(
    "button",
    { type: "button", class: "view-button", "aria-label": label, title: label, onClick },
    icon(name),
  );
}

function isTyping(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    (target.isContentEditable || target.matches("input:not([type='checkbox']), textarea, select"))
  );
}

function dialogOpen(): boolean {
  return document.querySelector("dialog[open]") !== null;
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "Something went wrong";
}
