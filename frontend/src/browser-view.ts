// 檔案瀏覽：路徑、工具列、檢視切換、檔案列表、右鍵選單、拖放上傳，以及右下角的上傳面板。
//
// 選取與列表的畫面在 file-list.ts；這裡負責「選了之後要做什麼」：改名、移動、刪除、下載、上傳。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { chooseDownload, nameDialog } from "./dialogs.js";
import { DRAG_TYPE, FileList, loadView, saveView, type ViewMode } from "./file-list.js";
import { icon, type IconName } from "./icons.js";
import { openContextMenu, type MenuEntry } from "./menu.js";
import { chooseFolder } from "./move-dialog.js";
import { PendingUploads } from "./pending-uploads.js";
import {
  defaultDirection,
  loadSort,
  saveSort,
  sortItems,
  type SortKey,
  type SortOptions,
} from "./sort.js";
import { sortControl, type SortControl } from "./sort-menu.js";
import { folderHref } from "./router.js";
import { refreshStorage } from "./sidebar.js";
import { showToast } from "./toast.js";
import type { Crumb, FileItem, ItemDetail, ServerConfig } from "./types.js";
import { UploadQueue } from "./upload-queue.js";

/**
 * 導覽序號。每次載入資料夾時遞增，回應到達時比對：不是最新的就丟棄。
 * 否則使用者快速點了 A 再點 B，若 A 的回應較晚到，畫面會停在 A，網址卻顯示 B。
 */
let navigation = 0;

/** 目前顯示的資料夾畫面；不在檔案頁面時為 null */
let current: FolderView | null = null;

/**
 * 重新載入之後要顯示的錯誤。批次操作（刪除、移動）結束後會重新載入整個畫面，
 * 錯誤訊息要出現在新的畫面上，而不是隨著舊畫面一起消失。
 */
let errorAfterReload: string | null = null;

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
    refreshStorage();
    if (current !== null && current.folderId === folderId && !current.isBusy()) {
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

// 上傳進行中關閉分頁或重新整理，上傳會直接中斷。呼叫 preventDefault 後，瀏覽器會跳出內建的確認視窗。
// 視窗的文字由瀏覽器決定，網頁無法自訂（避免被濫用來欺騙使用者）。
window.addEventListener("beforeunload", (event) => {
  if (queue.isBusy()) {
    event.preventDefault();
  }
});

/**
 * 把右下角的兩個面板放進容器。只在程式啟動時呼叫一次。
 * 面板的高度會變（展開、縮小、多一個檔案），內容區底部要留出同樣的空間，
 * 最後一列檔案才能捲到面板上方，不會被擋住。
 */
export function mountPanels(container: HTMLElement): void {
  container.append(pending.element, queue.element);
  const observer = new ResizeObserver(() => {
    const height = container.hidden ? 0 : container.offsetHeight;
    document.documentElement.style.setProperty("--panels-height", `${height}px`);
  });
  observer.observe(container);
}

/** 在 container 中顯示資料夾的內容。folderId 為 null 代表根目錄 */
export function renderBrowser(container: HTMLElement, folderId: number | null): void {
  current?.unmount();
  current = null;
  void load(container, folderId);
}

/** 離開檔案頁面時呼叫：移除鍵盤與拖放的監聽 */
export function leaveBrowser(): void {
  navigation += 1;
  current?.unmount();
  current = null;
}

/** 左側欄的 New → New folder */
export function newFolder(): void {
  current?.createFolder();
}

/** 左側欄的 New → Upload files */
export function chooseFiles(): void {
  current?.chooseFiles();
}

async function load(container: HTMLElement, folderId: number | null): Promise<void> {
  const token = ++navigation;
  container.replaceChildren(h("p", { class: "muted" }, "Loading…"));

  try {
    // 列表與路徑同時請求
    const [items, detail] = await Promise.all([
      api.get<FileItem[]>(folderId === null ? "/api/files" : `/api/files?parent_id=${folderId}`),
      folderId === null ? Promise.resolve(null) : api.get<ItemDetail>(`/api/files/${folderId}`),
    ]);

    // 等待期間使用者已經切換到別的資料夾：這份結果已經過時
    if (token !== navigation) {
      return;
    }

    current = new FolderView(container, folderId, items, detail);
    current.mount();
    // 未完成的清單不屬於任何資料夾，但每次載入時更新，使用者總是看到最新的狀態
    pending.refresh();
    refreshStorage();
  } catch (error) {
    if (token !== navigation || isHandledGlobally(error)) {
      return;
    }
    container.replaceChildren(
      h("p", { class: "error", role: "alert" }, messageOf(error)),
      h("p", {}, h("a", { href: folderHref(null) }, "Back to My files")),
    );
  }
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "Something went wrong";
}

// ==================== 路徑 ====================

/** My files / 相片 / 2026。目前所在的那一層不是連結 */
function breadcrumbs(detail: ItemDetail | null): HTMLElement {
  const crumbs: Crumb[] = detail ? [...detail.ancestors, { id: detail.id, name: detail.name }] : [];

  // 上層的每一層都可以接受拖曳過來的項目（搬回上層）；目前所在的那一層不行
  const root =
    crumbs.length === 0
      ? h("span", { "aria-current": "page" }, "My files")
      : h(
          "a",
          { href: folderHref(null), "data-drop-folder": "root", "data-drop-name": "My files" },
          "My files",
        );

  const items = crumbs.map((crumb, index) =>
    h(
      "li",
      {},
      index === crumbs.length - 1
        ? h("span", { "aria-current": "page" }, crumb.name)
        : h(
            "a",
            {
              href: folderHref(crumb.id),
              "data-drop-folder": crumb.id,
              "data-drop-name": crumb.name,
            },
            crumb.name,
          ),
    ),
  );

  return h(
    "nav",
    { class: "breadcrumbs", "aria-label": "Current location" },
    h("ol", {}, h("li", {}, root), items),
  );
}

// ==================== 資料夾畫面 ====================

class FolderView {
  private readonly errorBox = h("p", { class: "error", role: "alert" });
  private readonly picker: HTMLInputElement;
  private readonly overlay: HTMLElement;
  private list: FileList;
  private view: ViewMode = loadView();
  private sort: SortOptions = loadSort();
  /** 排序按鈕與選單：只建立一次，交給列表放在 Modified 的右邊（Grid 時放在方塊上方） */
  private readonly sorter: SortControl;
  /** 鍵盤與拖放的監聽，離開這個畫面時一次移除 */
  private readonly controller = new AbortController();
  /** 操作進行中，避免重複送出（例如連點兩次刪除） */
  private busy = false;
  /** 拖曳進入與離開的次數差：經過子元素時也會觸發 dragleave，用計數判斷是否真的離開 */
  private dragDepth = 0;
  /** 拖放提示中的目的地名稱：拖到某個資料夾上時換成那個資料夾 */
  private readonly overlayTarget: HTMLElement;
  /** 拖曳時反白的資料夾 */
  private hovered: HTMLElement | null = null;

  private readonly buttons: {
    rename: HTMLButtonElement;
    copy: HTMLButtonElement;
    move: HTMLButtonElement;
    remove: HTMLButtonElement;
    download: HTMLButtonElement;
    upload: HTMLButtonElement;
  };
  private readonly viewButtons: { list: HTMLButtonElement; grid: HTMLButtonElement };

  constructor(
    readonly container: HTMLElement,
    readonly folderId: number | null,
    private readonly items: readonly FileItem[],
    private readonly detail: ItemDetail | null,
  ) {
    // 隱藏的檔案選擇器，由 Upload 按鈕與左側欄的 New 觸發
    this.picker = h("input", { type: "file", multiple: true, hidden: true });
    this.picker.addEventListener("change", () => {
      if (this.picker.files !== null && this.picker.files.length > 0) {
        queue.add(Array.from(this.picker.files), this.folderId);
      }
      // 清空，否則再次選擇同一個檔案時不會觸發 change
      this.picker.value = "";
    });

    this.buttons = {
      rename: toolButton("Rename (F2)", "rename", () => this.list.startRename()),
      copy: toolButton("Copy", "copy", () => void this.copySelected()),
      move: toolButton("Move", "move", () => void this.moveSelected()),
      remove: toolButton("Delete", "trash", () => void this.deleteSelected()),
      download: toolButton("Download", "download", () => void this.downloadSelected()),
      upload: toolButton("Upload files", "upload", () => this.chooseFiles()),
    };
    this.viewButtons = {
      list: viewButton("List view", "list", () => this.setView("list")),
      grid: viewButton("Grid view", "grid", () => this.setView("grid")),
    };

    this.overlayTarget = h("strong", {}, this.locationName());
    this.overlay = h(
      "div",
      { class: "drop-overlay", "aria-hidden": "true", hidden: true },
      h(
        "div",
        { class: "drop-label" },
        icon("upload", 22),
        h("span", {}, "Drop files to upload to ", this.overlayTarget),
      ),
    );

    this.sorter = sortControl(this.sort, (sort) => this.setSort(sort));
    this.list = this.createList();
  }

  mount(): void {
    this.container.replaceChildren(
      h(
        "div",
        { class: "folder-header" },
        breadcrumbs(this.detail),
        h(
          "div",
          { class: "toolbar", role: "toolbar", "aria-label": "File actions" },
          this.buttons.rename,
          this.buttons.copy,
          this.buttons.move,
          this.buttons.remove,
          this.buttons.download,
          this.buttons.upload,
        ),
        h(
          "div",
          { class: "view-switch", role: "group", "aria-label": "View" },
          this.viewButtons.list,
          this.viewButtons.grid,
        ),
      ),
      this.errorBox,
      this.list.element,
      this.overlay,
      this.picker,
    );
    if (errorAfterReload !== null) {
      this.errorBox.textContent = errorAfterReload;
      errorAfterReload = null;
    }
    this.updateViewButtons();
    this.updateToolbar();
    this.bindKeys();
    this.bindDrop();
    // 框選可以從內容區的任何空白處開始，不只是列表裡面
    this.container.addEventListener("mousedown", (event) => this.list.startMarquee(event), {
      signal: this.controller.signal,
    });
  }

  unmount(): void {
    this.controller.abort();
  }

  isBusy(): boolean {
    return this.busy || this.list.isRenaming();
  }

  /** New folder：與 File Browser 相同的對話框，同名等錯誤顯示在對話框裡，改好再按一次就好 */
  createFolder(): void {
    void (async () => {
      const created = await nameDialog({
        title: "New folder",
        message: "Name your new folder.",
        confirmLabel: "Create",
        location: this.locationPath(),
        submit: async (name) => {
          await api.post("/api/folders", { name, parent_id: this.folderId });
        },
      });
      if (created) {
        renderBrowser(this.container, this.folderId);
      }
    })();
  }

  chooseFiles(): void {
    this.picker.click();
  }

  // ---------- 列表與檢視 ----------

  private createList(): FileList {
    return new FileList(sortItems(this.items, this.sort), {
      view: this.view,
      onOpen: (item) => this.open(item),
      onSelectionChange: () => this.updateToolbar(),
      onContextMenu: (x, y) => this.showContextMenu(x, y),
      onRename: (item, name) => this.rename(item, name),
      marqueeHost: this.container,
      sort: this.sort,
      onSort: (key) => this.sortBy(key),
      sortControl: this.sorter.element,
    });
  }

  /** 切換 List／Grid：保留目前的選取，選擇記在瀏覽器中 */
  private setView(view: ViewMode): void {
    if (view === this.view) {
      return;
    }
    this.view = view;
    saveView(view);
    this.rebuildList();
    this.updateViewButtons();
  }

  /** 點欄位標題：同一欄反轉方向；換到另一欄時用那一欄最常用的方向 */
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

  /** 依目前的檢視與排序重新產生列表，保留選取 */
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

  /** 依選取的數量啟用或停用工具列的按鈕 */
  private updateToolbar(): void {
    const selected = this.list.selectedItems();
    const single = selected.length === 1 ? selected[0] : undefined;
    const none = selected.length === 0;
    this.buttons.rename.disabled = single === undefined;
    this.buttons.copy.disabled = none;
    this.buttons.move.disabled = none;
    this.buttons.remove.disabled = none;
    this.buttons.download.disabled = none;
  }

  private open(item: FileItem): void {
    if (item.kind === "folder") {
      window.location.hash = folderHref(item.id);
    } else {
      download(item);
    }
  }

  // ---------- 右鍵選單 ----------

  private showContextMenu(x: number, y: number): void {
    const selected = this.list.selectedItems();
    const single = selected.length === 1 ? selected[0] : undefined;
    const title = single?.name ?? `${selected.length} items selected`;

    const entries: (MenuEntry | "separator")[] = [];
    if (single !== undefined) {
      entries.push({
        label: "Rename",
        icon: "rename",
        shortcut: "F2",
        onSelect: () => this.list.startRename(single),
      });
    }
    entries.push(
      { label: "Copy…", icon: "copy", onSelect: () => void this.copySelected() },
      { label: "Move…", icon: "move", onSelect: () => void this.moveSelected() },
      { label: "Download", icon: "download", onSelect: () => void this.downloadSelected() },
      "separator",
      { label: "Delete", icon: "trash", danger: true, onSelect: () => void this.deleteSelected() },
    );
    openContextMenu(x, y, title, entries);
  }

  // ---------- 操作 ----------

  /** 原位改名：失敗時拋出錯誤，列表會恢復原本的名稱 */
  private async rename(item: FileItem, name: string): Promise<void> {
    this.errorBox.textContent = "";
    try {
      await api.patch(`/api/files/${item.id}`, { name });
    } catch (error) {
      if (!isHandledGlobally(error)) {
        this.errorBox.textContent = messageOf(error);
      }
      throw error;
    }
    renderBrowser(this.container, this.folderId);
  }

  /**
   * 下載選取的項目：
   * - 一個檔案：直接下載
   * - 一個資料夾：下載成 .zip
   * - 多個項目：讓使用者選擇一個 .zip 或分開下載（有資料夾時只能 .zip）
   */
  private async downloadSelected(): Promise<void> {
    const selected = this.list.selectedItems();
    const [first] = selected;
    if (first === undefined) {
      return;
    }
    if (selected.length === 1) {
      if (first.kind === "file") {
        download(first);
      } else {
        downloadArchive(selected);
      }
      return;
    }
    const choice = await chooseDownload(
      selected.length,
      selected.some((item) => item.kind === "folder"),
    );
    if (choice === "zip") {
      downloadArchive(selected);
    } else if (choice === "separate") {
      downloadEach(selected);
    }
  }

  private async copySelected(): Promise<void> {
    const selected = this.list.selectedItems();
    if (selected.length === 0) {
      return;
    }
    // 資料夾不能複製進它自己裡面
    const folders = new Set(selected.filter((item) => item.kind === "folder").map((item) => item.id));
    const title = selected.length === 1 ? `Copy "${selected[0]?.name ?? ""}"` : `Copy ${selected.length} items`;
    const target = await chooseFolder(title, "Copy here", this.folderId, folders);
    if (target === undefined) {
      return;
    }
    // 複製到同一個資料夾也可以：伺服器自動改名為「名稱 (1)」
    await this.each(selected, "Copied", (item) =>
      api.post(`/api/files/${item.id}/copy`, { parent_id: target }),
    );
  }

  /**
   * 刪除：放進垃圾桶，不需要確認（與 Google Drive 相同）。
   * 誤刪的代價很低：左下角的通知可以立刻 Undo，之後也能從垃圾桶還原
   */
  private async deleteSelected(): Promise<void> {
    const selected = this.list.selectedItems();
    if (selected.length === 0) {
      return;
    }
    const trashed: FileItem[] = [];
    await this.each(selected, "Deleted", async (item) => {
      await api.delete(`/api/files/${item.id}`);
      trashed.push(item);
    });

    const [first] = trashed;
    if (first === undefined) {
      return;
    }
    const message =
      trashed.length === 1 ? `"${first.name}" moved to trash` : `${trashed.length} items moved to trash`;
    showToast(message, { label: "Undo", onClick: () => void restoreItems(trashed) });
  }

  private async moveSelected(): Promise<void> {
    const selected = this.list.selectedItems();
    if (selected.length === 0) {
      return;
    }
    const folders = new Set(selected.filter((item) => item.kind === "folder").map((item) => item.id));
    const title = selected.length === 1 ? `Move "${selected[0]?.name ?? ""}"` : `Move ${selected.length} items`;
    const target = await chooseFolder(title, "Move here", this.folderId, folders);
    if (target === undefined || target === this.folderId) {
      return;
    }
    await this.each(selected, "Moved", (item) =>
      api.patch(`/api/files/${item.id}`, { parent_id: target }),
    );
  }

  /** 拖曳放下：把這些項目搬到 target。已經在那裡的項目略過 */
  private async moveTo(ids: ReadonlySet<number>, target: number | null): Promise<void> {
    const items = this.items.filter((item) => ids.has(item.id) && item.parent_id !== target);
    if (items.length === 0) {
      return;
    }
    await this.each(items, "Moved", (item) =>
      api.patch(`/api/files/${item.id}`, { parent_id: target }),
    );
  }

  /**
   * 對每個項目依序執行一個操作，結束後重新載入。
   * 有失敗時不中斷其他項目，最後說明成功了幾個以及第一個錯誤的原因。
   */
  private async each(
    items: readonly FileItem[],
    verb: string,
    action: (item: FileItem) => Promise<unknown>,
  ): Promise<void> {
    if (this.busy) {
      return;
    }
    this.busy = true;
    this.errorBox.textContent = "";
    let succeeded = 0;
    let firstError: unknown = null;

    for (const item of items) {
      try {
        await action(item);
        succeeded += 1;
      } catch (error) {
        if (isHandledGlobally(error)) {
          this.busy = false;
          return;
        }
        firstError ??= error;
      }
    }

    this.busy = false;
    if (firstError !== null) {
      errorAfterReload =
        items.length > 1
          ? `${verb} ${succeeded} of ${items.length} items. ${messageOf(firstError)}`
          : messageOf(firstError);
    }
    refreshStorage();
    renderBrowser(this.container, this.folderId);
  }

  // ---------- 鍵盤 ----------

  /** 整個頁面的快捷鍵：Ctrl／⌘＋A 全選、Esc 取消選取、F2 改名 */
  private bindKeys(): void {
    window.addEventListener(
      "keydown",
      (event) => {
        // 選單開著時，Esc 由選單處理（已經呼叫 preventDefault）
        if (event.defaultPrevented || isTyping(event.target) || dialogOpen()) {
          return;
        }
        const mod = event.metaKey || event.ctrlKey;
        if (mod && event.key.toLowerCase() === "a") {
          event.preventDefault();
          this.list.selectAll();
        } else if (event.key === "Escape") {
          this.list.clear();
        } else if (event.key === "F2") {
          event.preventDefault();
          this.list.startRename();
        }
      },
      { signal: this.controller.signal },
    );
  }

  // ---------- 拖放：從電腦拖進來的檔案上傳，站內的項目搬移 ----------

  /**
   * 兩種拖放共用同一組監聽：
   * - 從電腦拖進來的檔案：顯示提示，放下就上傳到目前的資料夾；放在某個資料夾上，就上傳到那個資料夾
   * - 列表中的項目：只能放在資料夾（列表中的資料夾、上方路徑的上層）上，放下就搬移過去
   */
  private bindDrop(): void {
    const { signal } = this.controller;
    const zone = this.container;

    zone.addEventListener(
      "dragenter",
      (event) => {
        if (hasFiles(event)) {
          event.preventDefault();
          this.dragDepth += 1;
          this.overlay.hidden = false;
        }
      },
      { signal },
    );
    zone.addEventListener("dragover", (event) => this.dragOver(event), { signal });
    zone.addEventListener(
      "dragleave",
      () => {
        if (this.dragDepth > 0) {
          this.dragDepth -= 1;
          if (this.dragDepth === 0) {
            this.overlay.hidden = true;
            this.highlight(null);
          }
        }
      },
      { signal },
    );
    zone.addEventListener("drop", (event) => this.drop(event), { signal });
    // 拖到視窗外放開、或按 Esc 取消時不會有 drop：提示與反白都要收起來
    window.addEventListener("dragend", () => this.resetDrag(), { signal });
  }

  private dragOver(event: DragEvent): void {
    const target = this.dropTarget(event);

    if (isInternal(event)) {
      const valid = target !== null && this.canMoveTo(target.id);
      this.highlight(valid ? target.element : null);
      if (valid) {
        // 取消預設行為，瀏覽器才允許在這裡放下
        event.preventDefault();
        if (event.dataTransfer !== null) {
          event.dataTransfer.dropEffect = "move";
        }
      }
      return;
    }

    if (!hasFiles(event)) {
      return;
    }
    event.preventDefault();
    if (event.dataTransfer !== null) {
      event.dataTransfer.dropEffect = "copy";
    }
    this.overlay.hidden = false;
    this.highlight(target?.element ?? null);
    this.overlayTarget.textContent = target?.name ?? this.locationName();
  }

  private drop(event: DragEvent): void {
    event.preventDefault();
    const target = this.dropTarget(event);
    const internal = isInternal(event);
    const dragging = this.list.draggingIds();
    this.resetDrag();

    if (internal) {
      if (target !== null && dragging !== null && this.canMoveTo(target.id)) {
        void this.moveTo(dragging, target.id);
      }
      return;
    }
    this.handleDrop(event, target === null ? this.folderId : target.id);
  }

  /** 拖曳中的項目可以放到這個資料夾嗎：不能放進自己，也不能「搬到目前所在的資料夾」 */
  private canMoveTo(target: number | null): boolean {
    const dragging = this.list.draggingIds();
    if (dragging === null || target === this.folderId) {
      return false;
    }
    return target === null || !dragging.has(target);
  }

  /** 游標下方可以接受放下的資料夾（列表中的資料夾或路徑的某一層）；沒有時為 null */
  private dropTarget(event: DragEvent): { element: HTMLElement; id: number | null; name: string } | null {
    const node = event.target;
    if (!(node instanceof Element)) {
      return null;
    }
    const element = node.closest<HTMLElement>("[data-drop-folder]");
    if (element === null || !this.container.contains(element)) {
      return null;
    }
    const raw = element.dataset.dropFolder;
    const id = raw === "root" ? null : Number(raw);
    if (id !== null && !Number.isSafeInteger(id)) {
      return null;
    }
    return { element, id, name: element.dataset.dropName ?? "My files" };
  }

  private highlight(element: HTMLElement | null): void {
    if (this.hovered === element) {
      return;
    }
    this.hovered?.classList.remove("drop-hover");
    element?.classList.add("drop-hover");
    this.hovered = element;
  }

  private resetDrag(): void {
    this.dragDepth = 0;
    this.overlay.hidden = true;
    this.overlayTarget.textContent = this.locationName();
    this.highlight(null);
  }

  /**
   * 處理放下的檔案。瀏覽器會把拖進來的資料夾也當成一個「檔案」，
   * 送出時才會失敗，錯誤訊息還會誤導成「無法連線」，所以在這裡先排除並說明。
   */
  private handleDrop(event: DragEvent, folderId: number | null): void {
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
      folders.length > 0 ? `Uploading folders isn't supported yet: ${folders.join(", ")}` : "";
    if (files.length > 0) {
      queue.add(files, folderId);
    }
  }

  /** New folder 對話框中顯示的位置：My files / 相片 / 2026 */
  private locationPath(): string[] {
    const detail = this.detail;
    return detail === null
      ? ["My files"]
      : ["My files", ...detail.ancestors.map((crumb) => crumb.name), detail.name];
  }

  /** 拖放提示中顯示的目的地：目前資料夾的名稱 */
  private locationName(): string {
    return this.detail?.name ?? "My files";
  }
}

// ==================== 輔助函式 ====================

/**
 * 通知的 Undo：把剛剛刪除的項目從垃圾桶還原，再重新載入目前的資料夾。
 * 失敗的項目仍在垃圾桶中，說明一下就好，使用者可以到垃圾桶自己還原
 */
async function restoreItems(items: readonly FileItem[]): Promise<void> {
  let failed = 0;
  for (const item of items) {
    try {
      await api.post(`/api/trash/${item.id}/restore`, {});
    } catch (error) {
      if (isHandledGlobally(error)) {
        return;
      }
      failed += 1;
    }
  }
  if (failed > 0) {
    errorAfterReload = `${failed} of ${items.length} items could not be restored. They are still in the trash.`;
  }
  if (current !== null) {
    renderBrowser(current.container, current.folderId);
  }
}

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

/**
 * 下載一個檔案。伺服器回 Content-Disposition: attachment，瀏覽器會存檔而不會離開這個頁面。
 * 用帶 download 屬性的連結，而不是改變 location：後者在上傳進行中會觸發「離開網頁」的確認。
 */
function download(item: FileItem): void {
  startDownload(`/api/files/${item.id}/content`);
}

/** 打包成一個 .zip 下載。伺服器一邊打包一邊傳送，不會先在磁碟上產生壓縮檔 */
function downloadArchive(items: readonly FileItem[]): void {
  startDownload(`/api/archive?ids=${items.map((item) => item.id).join(",")}`);
}

/** 分開下載每個檔案。間隔一點時間：瀏覽器對同時觸發的多個下載會擋掉一部分 */
function downloadEach(items: readonly FileItem[]): void {
  items.forEach((item, index) => {
    window.setTimeout(() => download(item), index * 400);
  });
}

function startDownload(href: string): void {
  const link = h("a", { href, download: "", hidden: true });
  document.body.append(link);
  link.click();
  link.remove();
}

/** 拖曳的是列表中的項目（搬移），而不是從電腦拖進來的檔案（上傳） */
function isInternal(event: DragEvent): boolean {
  return event.dataTransfer?.types.includes(DRAG_TYPE) ?? false;
}

function hasFiles(event: DragEvent): boolean {
  return event.dataTransfer?.types.includes("Files") ?? false;
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
