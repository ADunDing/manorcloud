// 管理員瀏覽某位使用者的檔案（唯讀）：#/browse/{user} 與 #/browse/{user}/folder/{id}
//
// 列表與 My files 共用 FileList：多選、框選、排序、List／Grid 的行為相同，
// 但只能開啟資料夾與下載，不能上傳、改名、搬移或刪除。
// 下載會記錄在伺服器的 log 中。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { chooseDownload } from "./dialogs.js";
import { h } from "./dom.js";
import { FileList, loadView, saveView, type ViewMode } from "./file-list.js";
import { icon, type IconName } from "./icons.js";
import { openContextMenu } from "./menu.js";
import { browseHref, folderHref, usersHref } from "./router.js";
import {
  defaultDirection,
  loadSort,
  saveSort,
  sortItems,
  type SortKey,
  type SortOptions,
} from "./sort.js";
import { sortControl, type SortControl } from "./sort-menu.js";
import type { Crumb, FileItem, ItemDetail, StorageOverview } from "./types.js";

/** 載入序號：離開或換到別的資料夾後，較慢回來的舊結果不能蓋掉新的畫面 */
let loading = 0;
let current: BrowseView | null = null;

/** 這位使用者在哪顆硬碟、哪個資料夾：顯示在路徑的最前面 */
interface Owner {
  readonly id: number;
  readonly username: string;
  /** 路徑的第一段：管理員自己的檔案所在的那一顆叫 My files，其他是硬碟的名稱 */
  readonly rootLabel: string;
  readonly rootHref: string;
  readonly volumeId: number | null;
  readonly folder: string;
}

/**
 * @param adminId 目前的管理員：判斷路徑的第一段是 My files 還是硬碟的名稱
 */
export function renderBrowse(
  container: HTMLElement,
  userId: number,
  folderId: number | null,
  adminId: number,
): void {
  current?.unmount();
  current = null;
  const token = ++loading;
  container.replaceChildren(h("p", { class: "muted" }, "Loading…"));

  void (async () => {
    try {
      const base = `/api/admin/users/${userId}`;
      const [storage, items, detail] = await Promise.all([
        api.get<StorageOverview>("/api/admin/storage"),
        api.get<FileItem[]>(
          folderId === null ? `${base}/files` : `${base}/files?parent_id=${folderId}`,
        ),
        folderId === null ? Promise.resolve(null) : api.get<ItemDetail>(`${base}/files/${folderId}`),
      ]);
      if (token !== loading) {
        return;
      }
      const user = storage.users.find((entry) => entry.user_id === userId);
      if (user === undefined) {
        container.replaceChildren(h("p", { class: "error", role: "alert" }, "User not found"));
        return;
      }
      const home = storage.users.find((entry) => entry.user_id === adminId)?.volume_id ?? null;
      const atHome = user.volume_id === home;
      const owner = {
        id: userId,
        username: user.username,
        rootLabel: atHome
          ? "My files"
          : (storage.volumes.find((v) => v.id === user.volume_id)?.name ?? "Unknown"),
        rootHref: atHome ? folderHref(null) : folderHref(null, user.volume_id),
        volumeId: user.volume_id,
        folder: user.folder,
      };
      current = new BrowseView(container, owner, folderId, items, detail);
      current.mount();
    } catch (error) {
      if (token !== loading || isHandledGlobally(error)) {
        return;
      }
      container.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
    }
  })();
}

/** 離開時呼叫：移除鍵盤的監聽，還在路上的回應不再更新畫面 */
export function leaveBrowse(): void {
  loading += 1;
  current?.unmount();
  current = null;
}

class BrowseView {
  private readonly controller = new AbortController();
  private readonly sorter: SortControl;
  private readonly downloadButton: HTMLButtonElement;
  private readonly viewButtons: { list: HTMLButtonElement; grid: HTMLButtonElement };
  private list: FileList;
  private view: ViewMode = loadView();
  /** 排序與 My files 共用同一個設定 */
  private sort: SortOptions = loadSort();

  constructor(
    private readonly container: HTMLElement,
    private readonly owner: Owner,
    private readonly folderId: number | null,
    private readonly items: readonly FileItem[],
    private readonly detail: ItemDetail | null,
  ) {
    this.downloadButton = h(
      "button",
      {
        type: "button",
        class: "icon-button",
        "aria-label": "Download",
        title: "Download",
        onClick: () => void this.downloadSelected(),
      },
      icon("download"),
    );
    this.viewButtons = {
      list: viewButton("List view", "list", () => this.setView("list")),
      grid: viewButton("Grid view", "grid", () => this.setView("grid")),
    };
    this.sorter = sortControl(this.sort, (sort) => this.setSort(sort));
    this.list = this.createList();
  }

  mount(): void {
    this.container.replaceChildren(
      h(
        "div",
        { class: "folder-header" },
        this.breadcrumbs(),
        h(
          "div",
          { class: "toolbar", role: "toolbar", "aria-label": "Actions" },
          this.downloadButton,
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
        h(
          "span",
          {},
          `You're viewing ${this.owner.username}'s files as an administrator. You can open and download them, but not change them.`,
        ),
      ),
      this.list.element,
    );
    this.updateViewButtons();
    this.updateToolbar();
    this.bindKeys();
    this.container.addEventListener("mousedown", (event) => this.list.startMarquee(event), {
      signal: this.controller.signal,
    });
  }

  unmount(): void {
    this.controller.abort();
  }

  /** Disk 2 / users / alice / 相片 / 2026。目前所在的那一層不是連結 */
  private breadcrumbs(): HTMLElement {
    const crumbs: Crumb[] = this.detail
      ? [...this.detail.ancestors, { id: this.detail.id, name: this.detail.name }]
      : [];
    const atRoot = crumbs.length === 0;
    const rootLabel = this.owner.folder;
    return h(
      "nav",
      { class: "breadcrumbs", "aria-label": "Current location" },
      h(
        "ol",
        {},
        h("li", {}, h("a", { href: this.owner.rootHref }, this.owner.rootLabel)),
        h("li", {}, h("a", { href: usersHref(this.owner.volumeId) }, "users")),
        h(
          "li",
          {},
          atRoot
            ? h("span", { "aria-current": "page" }, rootLabel)
            : h("a", { href: browseHref(this.owner.id) }, rootLabel),
        ),
        crumbs.map((crumb, index) =>
          h(
            "li",
            {},
            index === crumbs.length - 1
              ? h("span", { "aria-current": "page" }, crumb.name)
              : h("a", { href: browseHref(this.owner.id, crumb.id) }, crumb.name),
          ),
        ),
      ),
    );
  }

  // ---------- 列表 ----------

  private createList(): FileList {
    return new FileList(sortItems(this.items, this.sort), {
      view: this.view,
      onOpen: (item) => this.open(item),
      onSelectionChange: () => this.updateToolbar(),
      onContextMenu: (x, y) => this.showContextMenu(x, y),
      // 唯讀：不能改名
      onRename: async () => undefined,
      marqueeHost: this.container,
      sort: this.sort,
      onSort: (key) => this.sortBy(key),
      sortControl: this.sorter.element,
      draggable: false,
      emptyMessage: this.folderId === null ? "No files yet." : "This folder is empty.",
    });
  }

  private open(item: FileItem): void {
    if (item.kind === "folder") {
      window.location.hash = browseHref(this.owner.id, item.id);
    } else {
      this.download([item]);
    }
  }

  private showContextMenu(x: number, y: number): void {
    const selected = this.list.selectedItems();
    const [first] = selected;
    if (first === undefined) {
      return;
    }
    const title = selected.length === 1 ? first.name : `${selected.length} items selected`;
    openContextMenu(x, y, title, [
      ...(selected.length === 1 && first.kind === "folder"
        ? [{ label: "Open", icon: "folder" as const, onSelect: () => this.open(first) }]
        : []),
      { label: "Download", icon: "download", onSelect: () => void this.downloadSelected() },
    ]);
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
    saveSort(sort);
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
    this.downloadButton.disabled = this.list.selectedItems().length === 0;
  }

  // ---------- 下載 ----------

  /** 與 My files 相同：一個檔案直接下載，資料夾或多個項目可以打包成 .zip */
  private async downloadSelected(): Promise<void> {
    const selected = this.list.selectedItems();
    const [first] = selected;
    if (first === undefined) {
      return;
    }
    if (selected.length === 1) {
      if (first.kind === "file") {
        this.download([first]);
      } else {
        this.archive(selected);
      }
      return;
    }
    const choice = await chooseDownload(
      selected.length,
      selected.some((item) => item.kind === "folder"),
    );
    if (choice === "zip") {
      this.archive(selected);
    } else if (choice === "separate") {
      this.download(selected);
    }
  }

  /** 分開下載每個檔案。間隔一點時間：瀏覽器對同時觸發的多個下載會擋掉一部分 */
  private download(items: readonly FileItem[]): void {
    items.forEach((item, index) => {
      window.setTimeout(
        () => startDownload(`/api/admin/users/${this.owner.id}/files/${item.id}/content`),
        index * 400,
      );
    });
  }

  private archive(items: readonly FileItem[]): void {
    const ids = items.map((item) => item.id).join(",");
    startDownload(`/api/admin/users/${this.owner.id}/archive?ids=${ids}`);
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

function viewButton(label: string, name: IconName, onClick: () => void): HTMLButtonElement {
  return h(
    "button",
    { type: "button", class: "view-button", "aria-label": label, title: label, onClick },
    icon(name),
  );
}

function startDownload(href: string): void {
  const link = h("a", { href, download: "", hidden: true });
  document.body.append(link);
  link.click();
  link.remove();
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
