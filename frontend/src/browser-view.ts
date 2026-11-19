// 檔案瀏覽：路徑、工具列、檢視切換、檔案列表、右鍵選單、拖放上傳，以及右下角的上傳面板。
//
// 選取與列表的畫面在 file-list.ts；這裡負責「選了之後要做什麼」：改名、複製、移動、刪除、下載、上傳、預覽圖片（preview.ts）。
//
// My files 與管理員看到的每一顆硬碟都用這個畫面，差別只在 API 的 space 參數（見 space.ts）。
// 資料夾以路徑表示（例如 相片/2026），項目以名稱識別：名稱就是硬碟上真實的檔名。
// 硬碟的最上層有一個 users 資料夾（每個人的 My files），固定在列表最前面。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { chooseDownload, nameDialog } from "./dialogs.js";
import {
  DRAG_TYPE,
  FileList,
  loadView,
  saveView,
  type PinnedFolder,
  type ViewMode,
} from "./file-list.js";
import { icon, type IconName } from "./icons.js";
import { isInside, toFileItems } from "./listing.js";
import { openContextMenu, type MenuEntry } from "./menu.js";
import { chooseFolder, type FolderChoice, type FolderPlace } from "./move-dialog.js";
import { PendingUploads } from "./pending-uploads.js";
import { isImage, openPreview, thumbnailUrl } from "./preview.js";
import {
  defaultDirection,
  loadSort,
  saveSort,
  sortItems,
  type SortKey,
  type SortOptions,
} from "./sort.js";
import { sortControl, type SortControl } from "./sort-menu.js";
import { filesHref, MY_FILES, type Location } from "./router.js";
import { refreshStorage } from "./sidebar.js";
import { apiUrl, joinPath, spaceOf, spaceOfLocation, type Space } from "./space.js";
import { showToast } from "./toast.js";
import type { FileItem, Listing, ServerConfig, StorageOverview } from "./types.js";
import { TransferPanel } from "./transfers.js";
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

/** 要顯示哪一個資料夾，由 main.ts 依網址決定 */
interface BrowserOptions {
  readonly location: Location;
  readonly path: readonly string[];
  /** 管理員：路徑的第一段用硬碟的名稱 */
  readonly admin: boolean;
  /** 目前的使用者 */
  readonly userId: number;
}

/** 目前畫面所在的一組檔案 */
interface Place {
  readonly location: Location;
  readonly space: Space;
  /** 路徑最前面的名稱：My files 或硬碟的名稱 */
  readonly rootLabel: string;
}

/** 目前的檔案空間。同時只有一個資料夾畫面，下載與還原等輔助函式都以它為準 */
let place: Place = { location: MY_FILES, space: null, rootLabel: "My files" };
/** 複製、移動時可以選擇的目的地：管理員是 My files 與每一顆硬碟，一般使用者只有 My files */
let places: readonly FolderPlace[] = [place];
/** space= 的值 → 顯示的名稱，給傳輸面板使用 */
const spaceLabels = new Map<string, string>([["me", "My files"]]);
/** 上次的選項：畫面內部重新載入時沿用 */
let lastOptions: BrowserOptions = { location: MY_FILES, path: [], admin: false, userId: 0 };

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
  (dir, space) => {
    refreshStorage();
    if (current !== null && joinPath(current.path) === dir && place.space === space && !current.isBusy()) {
      reload();
    }
  },
  uploadLimit,
  // 分段上傳結束（完成、失敗或取消）：未完成的清單可能變了
  () => pending.refresh(),
);

/** 未完成的上傳清單，與佇列一樣在整個應用程式中只有一個 */
const pending = new PendingUploads(queue);

/** 跨硬碟的複製與移動。結束時重新載入目前的資料夾 */
const transfers = new TransferPanel(
  (space) => spaceLabels.get(space) ?? space,
  () => {
    refreshStorage();
    if (current !== null && !current.isBusy()) {
      reload();
    }
  },
);

// 上傳進行中關閉分頁或重新整理，上傳會直接中斷。呼叫 preventDefault 後，瀏覽器會跳出內建的確認視窗。
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
  container.append(transfers.element, pending.element, queue.element);
  const observer = new ResizeObserver(() => {
    const height = container.hidden ? 0 : container.offsetHeight;
    document.documentElement.style.setProperty("--panels-height", `${height}px`);
  });
  observer.observe(container);
}

/** 在 container 中顯示資料夾的內容 */
export function renderBrowser(container: HTMLElement, options: BrowserOptions): void {
  lastOptions = options;
  current?.unmount();
  current = null;
  void load(container, options);
}

/** 重新載入目前的資料夾 */
function reload(): void {
  if (current !== null) {
    renderBrowser(current.container, lastOptions);
  }
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

async function load(container: HTMLElement, options: BrowserOptions): Promise<void> {
  const token = ++navigation;
  container.replaceChildren(h("p", { class: "muted" }, "Loading…"));
  const space = spaceOfLocation(options.location);

  try {
    const [listing, rootLabel, choices] = await Promise.all([
      api.get<Listing>(apiUrl("/api/files", space, { path: joinPath(options.path) })),
      rootLabelOf(options),
      placesFor(options),
    ]);

    // 等待期間使用者已經切換到別的資料夾：這份結果已經過時
    if (token !== navigation) {
      return;
    }

    place = { location: options.location, space, rootLabel };
    places = choices.some((choice) => choice.space === space) ? choices : [place, ...choices];
    current = new FolderView(container, options.path, listing);
    current.mount();
    // 未完成的清單不屬於任何資料夾，但每次載入時更新，使用者總是看到最新的狀態
    pending.refresh(place.space);
    transfers.refresh();
    refreshStorage();
  } catch (error) {
    if (token !== navigation || isHandledGlobally(error)) {
      return;
    }
    container.replaceChildren(
      h("p", { class: "error", role: "alert" }, messageOf(error)),
      h("p", {}, h("a", { href: filesHref(MY_FILES) }, "Back to My files")),
    );
  }
}

/** 路徑的第一段：My files 或硬碟的名稱 */
async function rootLabelOf(options: BrowserOptions): Promise<string> {
  const location = options.location;
  switch (location.kind) {
    case "me":
      return "My files";
    case "disk": {
      const storage = await api.get<StorageOverview>("/api/admin/storage");
      return storage.volumes.find((volume) => volume.id === location.disk)?.name ?? "Disk";
    }
  }
}

/** 複製、移動的目的地。一般使用者只有自己的 My files */
async function placesFor(options: BrowserOptions): Promise<FolderPlace[]> {
  const mine: FolderPlace = { space: null, rootLabel: "My files" };
  if (!options.admin) {
    return [mine];
  }
  try {
    const storage = await api.get<StorageOverview>("/api/admin/storage");
    const home = storage.users.find((user) => user.user_id === options.userId)?.volume_id ?? null;
    const disks = storage.volumes
      .filter((volume) => volume.id !== home)
      .map((volume) => ({ space: spaceOf(volume.id), rootLabel: volume.name }));
    for (const disk of disks) {
      spaceLabels.set(disk.space, disk.rootLabel);
    }
    spaceLabels.set(spaceOf(home), "My files");
    return [mine, ...disks];
  } catch {
    return [mine];
  }
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "Something went wrong";
}

// ==================== 路徑 ====================

/** My files / 相片 / 2026。目前所在的那一層不是連結；上層可以接受拖曳過來的項目 */
function breadcrumbs(path: readonly string[]): HTMLElement {
  const all = [place.rootLabel, ...path];
  return h(
    "nav",
    { class: "breadcrumbs", "aria-label": "Current location" },
    h(
      "ol",
      {},
      all.map((name, index) =>
        h(
          "li",
          {},
          index === all.length - 1
            ? h("span", { "aria-current": "page" }, name)
            : h(
                "a",
                {
                  href: filesHref(place.location, path.slice(0, index)),
                  "data-drop-folder": `up:${index}`,
                  "data-drop-name": name,
                },
                name,
              ),
        ),
      ),
    ),
  );
}

// ==================== 資料夾畫面 ====================

class FolderView {
  private readonly errorBox = h("p", { class: "error", role: "alert" });
  private readonly picker: HTMLInputElement;
  private readonly overlay: HTMLElement;
  private readonly items: readonly FileItem[];
  /** 固定在最前面的資料夾（硬碟最上層的 users） */
  private readonly pinned: readonly PinnedFolder[];
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
    readonly path: readonly string[],
    private readonly listing: Listing,
  ) {
    this.items = toFileItems(listing.items.filter((item) => !item.locked));
    this.pinned = listing.items
      .filter((item) => item.locked)
      .map((item) => ({
        name: item.name,
        href: filesHref(place.location, [...path, item.name]),
        note: "People's files",
      }));

    // 隱藏的檔案選擇器，由 Upload 按鈕與左側欄的 New 觸發
    this.picker = h("input", { type: "file", multiple: true, hidden: true });
    this.picker.addEventListener("change", () => {
      if (this.picker.files !== null && this.picker.files.length > 0) {
        queue.add(Array.from(this.picker.files), joinPath(this.path), place.space);
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

  /** 這個資料夾不能修改時的原因 */
  private get readOnly(): string | null {
    return this.listing.read_only;
  }

  mount(): void {
    const tools = this.readOnly === null
      ? [
          this.buttons.rename,
          this.buttons.copy,
          this.buttons.move,
          this.buttons.remove,
          this.buttons.download,
          this.buttons.upload,
        ]
      : [this.buttons.copy, this.buttons.download];
    this.container.replaceChildren(
      h(
        "div",
        { class: "folder-header" },
        breadcrumbs(this.path),
        h("div", { class: "toolbar", role: "toolbar", "aria-label": "File actions" }, tools),
        h(
          "div",
          { class: "view-switch", role: "group", "aria-label": "View" },
          this.viewButtons.list,
          this.viewButtons.grid,
        ),
      ),
      ...(this.readOnly === null
        ? []
        : [
            h(
              "div",
              { class: "trash-banner" },
              icon("lock", 18),
              h("span", {}, `${this.readOnly}.`),
            ),
          ]),
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

  /** New folder：同名等錯誤顯示在對話框裡，改好再按一次就好 */
  createFolder(): void {
    if (this.readOnly !== null) {
      showToast(this.readOnly);
      return;
    }
    void (async () => {
      const created = await nameDialog({
        title: "New folder",
        message: "Name your new folder.",
        confirmLabel: "Create",
        location: [place.rootLabel, ...this.path],
        submit: async (name) => {
          await api.post(apiUrl("/api/folders", place.space), {
            path: joinPath(this.path),
            name,
          });
        },
      });
      if (created) {
        reload();
      }
    })();
  }

  chooseFiles(): void {
    if (this.readOnly !== null) {
      showToast(this.readOnly);
      return;
    }
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
      pinned: this.pinned,
      thumbnail: (item) =>
        isImage(item) ? thumbnailUrl(place.space, this.itemPath(item), item, "small") : null,
      draggable: this.readOnly === null,
      ...(this.readOnly === null
        ? {}
        : { emptyMessage: "This folder is empty." }),
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
    const none = selected.length === 0;
    this.buttons.rename.disabled = selected.length !== 1;
    this.buttons.copy.disabled = none;
    this.buttons.move.disabled = none;
    this.buttons.remove.disabled = none;
    this.buttons.download.disabled = none;
  }

  private itemPath(item: FileItem): string {
    return joinPath(this.path, item.name);
  }

  private open(item: FileItem): void {
    if (item.kind === "folder") {
      window.location.hash = filesHref(place.location, [...this.path, item.name]);
    } else if (isImage(item)) {
      this.preview(item);
    } else {
      download(this.itemPath(item), item.name);
    }
  }

  /** 預覽圖片：可以切換這個資料夾中的其他圖片，順序與列表相同 */
  private preview(item: FileItem): void {
    const images = sortItems(this.items, this.sort)
      .filter(isImage)
      .map((image) => ({ item: image, path: this.itemPath(image) }));
    const start = images.findIndex((entry) => entry.item.name === item.name);
    openPreview(place.space, images, start, (entry) => download(entry.path, entry.item.name));
  }

  // ---------- 右鍵選單 ----------

  private showContextMenu(x: number, y: number): void {
    const selected = this.list.selectedItems();
    const single = selected.length === 1 ? selected[0] : undefined;
    const title = single?.name ?? `${selected.length} items selected`;
    const writable = this.readOnly === null;

    const entries: (MenuEntry | "separator")[] = [];
    if (single?.kind === "folder") {
      entries.push({ label: "Open", icon: "folder", onSelect: () => this.open(single) });
    } else if (single !== undefined && isImage(single)) {
      entries.push({ label: "Preview", icon: "image", onSelect: () => this.preview(single) });
    }
    if (single !== undefined && writable) {
      entries.push({
        label: "Rename",
        icon: "rename",
        shortcut: "F2",
        onSelect: () => this.list.startRename(single),
      });
    }
    entries.push({ label: "Copy…", icon: "copy", onSelect: () => void this.copySelected() });
    if (writable) {
      entries.push({ label: "Move…", icon: "move", onSelect: () => void this.moveSelected() });
    }
    entries.push({
      label: "Download",
      icon: "download",
      onSelect: () => void this.downloadSelected(),
    });
    if (writable) {
      entries.push("separator", {
        label: "Delete",
        icon: "trash",
        danger: true,
        onSelect: () => void this.deleteSelected(),
      });
    }
    openContextMenu(x, y, title, entries);
  }

  // ---------- 操作 ----------

  /** 原位改名：失敗時拋出錯誤，列表會恢復原本的名稱 */
  private async rename(item: FileItem, name: string): Promise<void> {
    this.errorBox.textContent = "";
    try {
      await api.patch(apiUrl("/api/files", place.space), { path: this.itemPath(item), name });
    } catch (error) {
      if (!isHandledGlobally(error)) {
        this.errorBox.textContent = messageOf(error);
      }
      throw error;
    }
    reload();
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
        download(this.itemPath(first), first.name);
      } else {
        downloadArchive(this.path, selected);
      }
      return;
    }
    const choice = await chooseDownload(
      selected.length,
      selected.some((item) => item.kind === "folder"),
    );
    if (choice === "zip") {
      downloadArchive(this.path, selected);
    } else if (choice === "separate") {
      selected.forEach((item, index) => {
        window.setTimeout(() => download(this.itemPath(item), item.name), index * 400);
      });
    }
  }

  /** 選取的資料夾的路徑：不能把它們複製或搬進自己裡面 */
  private selectedFolders(selected: readonly FileItem[]): string[][] {
    return selected
      .filter((item) => item.kind === "folder")
      .map((item) => [...this.path, item.name]);
  }

  private async copySelected(): Promise<void> {
    const selected = this.list.selectedItems();
    if (selected.length === 0) {
      return;
    }
    const title =
      selected.length === 1 ? `Copy "${selected[0]?.name ?? ""}"` : `Copy ${selected.length} items`;
    const choice = await chooseFolder(
      title,
      "Copy here",
      this.path,
      this.selectedFolders(selected),
      place,
      places,
    );
    if (choice === undefined) {
      return;
    }
    if (choice.place.space !== place.space) {
      await this.transfer("copy", selected, choice);
      return;
    }
    // 複製到同一個資料夾也可以：伺服器自動改名為「名稱 (1)」
    await this.each(selected, "Copied", (item) =>
      api.post(apiUrl("/api/files/copy", place.space), {
        path: this.itemPath(item),
        parent: joinPath(choice.path),
      }),
    );
  }

  /**
   * 刪除：放進垃圾桶，不需要確認（與 Google Drive 相同）。
   * 誤刪的代價很低：左下角的通知可以立刻 Undo，之後也能從垃圾桶還原
   */
  private async deleteSelected(): Promise<void> {
    const selected = this.list.selectedItems();
    const [first] = selected;
    if (first === undefined) {
      return;
    }
    const trashed: number[] = [];
    await this.each(selected, "Deleted", async (item) => {
      const result = await api.delete<{ trash_id: number }>(
        apiUrl("/api/files", place.space, { path: this.itemPath(item) }),
      );
      trashed.push(result.trash_id);
    });

    if (trashed.length === 0) {
      return;
    }
    const message =
      trashed.length === 1
        ? `"${first.name}" moved to trash`
        : `${trashed.length} items moved to trash`;
    showToast(message, { label: "Undo", onClick: () => void restoreItems(trashed) });
  }

  private async moveSelected(): Promise<void> {
    const selected = this.list.selectedItems();
    if (selected.length === 0) {
      return;
    }
    const title =
      selected.length === 1 ? `Move "${selected[0]?.name ?? ""}"` : `Move ${selected.length} items`;
    const choice = await chooseFolder(
      title,
      "Move here",
      this.path,
      this.selectedFolders(selected),
      place,
      places,
    );
    if (choice === undefined) {
      return;
    }
    if (choice.place.space !== place.space) {
      await this.transfer("move", selected, choice);
      return;
    }
    if (joinPath(choice.path) === joinPath(this.path)) {
      return;
    }
    await this.moveTo(selected, choice.path);
  }

  /**
   * 複製或移動到另一顆硬碟：交給伺服器在背景進行，右下角的面板顯示進度。
   * 移動時，伺服器先完整複製，確認之後才刪除這裡的項目
   */
  private async transfer(
    kind: "copy" | "move",
    items: readonly FileItem[],
    choice: FolderChoice,
  ): Promise<void> {
    if (this.busy) {
      return;
    }
    this.errorBox.textContent = "";
    try {
      await api.post("/api/transfers", {
        kind,
        from_space: place.space ?? "me",
        from_path: joinPath(this.path),
        names: items.map((item) => item.name),
        to_space: choice.place.space ?? "me",
        to_path: joinPath(choice.path),
      });
    } catch (error) {
      if (!isHandledGlobally(error)) {
        this.errorBox.textContent = messageOf(error);
      }
      return;
    }
    const what = items.length === 1 ? `"${items[0]?.name ?? ""}"` : `${items.length} items`;
    showToast(`${kind === "move" ? "Moving" : "Copying"} ${what} to ${choice.place.rootLabel}`);
    transfers.refresh();
  }

  /** 把這些項目搬到 target */
  private async moveTo(items: readonly FileItem[], target: readonly string[]): Promise<void> {
    await this.each(items, "Moved", (item) =>
      api.patch(apiUrl("/api/files", place.space), {
        path: this.itemPath(item),
        parent: joinPath(target),
      }),
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
    reload();
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
        } else if (event.key === "F2" && this.readOnly === null) {
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
    if (this.readOnly !== null) {
      return;
    }
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
      const valid = target !== null && this.canMoveTo(target.path);
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
    const dragging = this.draggingItems();
    this.resetDrag();

    if (internal) {
      if (target !== null && dragging.length > 0 && this.canMoveTo(target.path)) {
        void this.moveTo(dragging, target.path);
      }
      return;
    }
    this.handleDrop(event, target === null ? this.path : target.path);
  }

  private draggingItems(): FileItem[] {
    const ids = this.list.draggingIds();
    return ids === null ? [] : this.items.filter((item) => ids.has(item.id));
  }

  /** 拖曳中的項目可以放到這個資料夾嗎：不能放進自己，也不能「搬到目前所在的資料夾」 */
  private canMoveTo(target: readonly string[]): boolean {
    const dragging = this.draggingItems();
    if (dragging.length === 0 || joinPath(target) === joinPath(this.path)) {
      return false;
    }
    return !dragging.some(
      (item) => item.kind === "folder" && isInside(target, [...this.path, item.name]),
    );
  }

  /** 游標下方可以接受放下的資料夾（列表中的資料夾或路徑的某一層）；沒有時為 null */
  private dropTarget(
    event: DragEvent,
  ): { element: HTMLElement; path: readonly string[]; name: string } | null {
    const node = event.target;
    if (!(node instanceof Element)) {
      return null;
    }
    const element = node.closest<HTMLElement>("[data-drop-folder]");
    if (element === null || !this.container.contains(element)) {
      return null;
    }
    const raw = element.dataset.dropFolder ?? "";
    const name = element.dataset.dropName ?? place.rootLabel;
    // 路徑的上層：up:{深度}
    const up = /^up:(\d+)$/.exec(raw)?.[1];
    if (up !== undefined) {
      return { element, path: this.path.slice(0, Number(up)), name };
    }
    const item = this.items.find((entry) => String(entry.id) === raw && entry.kind === "folder");
    return item === undefined
      ? null
      : { element, path: [...this.path, item.name], name: item.name };
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
  private handleDrop(event: DragEvent, folder: readonly string[]): void {
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
      queue.add(files, joinPath(folder), place.space);
    }
  }

  /** 拖放提示中顯示的目的地：目前資料夾的名稱 */
  private locationName(): string {
    return this.path[this.path.length - 1] ?? place.rootLabel;
  }
}

// ==================== 輔助函式 ====================

/**
 * 通知的 Undo：把剛剛刪除的項目從垃圾桶還原，再重新載入目前的資料夾。
 * 失敗的項目仍在垃圾桶中，說明一下就好，使用者可以到垃圾桶自己還原
 */
async function restoreItems(ids: readonly number[]): Promise<void> {
  let failed = 0;
  for (const id of ids) {
    try {
      await api.post(apiUrl(`/api/trash/${id}/restore`, place.space), {});
    } catch (error) {
      if (isHandledGlobally(error)) {
        return;
      }
      failed += 1;
    }
  }
  if (failed > 0) {
    errorAfterReload = `${failed} of ${ids.length} items could not be restored. They are still in the trash.`;
  }
  reload();
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
function download(path: string, name: string): void {
  startDownload(apiUrl("/api/files/content", place.space, { path }), name);
}

/** 打包成一個 .zip 下載。伺服器一邊打包一邊傳送，不會先在磁碟上產生壓縮檔 */
function downloadArchive(folder: readonly string[], items: readonly FileItem[]): void {
  startDownload(
    apiUrl("/api/archive", place.space, {
      path: joinPath(folder),
      names: items.map((item) => item.name).join("/"),
    }),
    "",
  );
}

function startDownload(href: string, name: string): void {
  const link = h("a", { href, download: name, hidden: true });
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
