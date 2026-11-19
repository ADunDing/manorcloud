// 檔案列表：List 與 Grid 兩種檢視。
//
// - 多選：點擊、Ctrl／⌘＋點擊、Shift＋點擊、拖曳框選、勾選框（觸控螢幕沒有 Ctrl 與 Shift）
// - 鍵盤：方向鍵移動、空白鍵切換選取、Enter 開啟、選單鍵或 Shift＋F10 開啟右鍵選單
// - 原位改名（startRename），拖曳調整 Name 與 Size 欄寬（寬度記在瀏覽器中）
// - 拖曳（與 Finder 相同）：List 中從圖示或檔名開始是「搬移」，從列的其他地方或空白處開始是「框選」；
//   Grid 中從方塊開始是搬移，從方塊之間的空隙開始是框選。
//   資料夾帶有 data-drop-folder，可以接受放下；放下後的處理在檔案瀏覽畫面
//
// 這個模組只負責畫面與選取；開啟、改名、右鍵選單要做的事，由建構時傳入的函式決定。
// My files 與 Trash 共用：垃圾桶不能拖曳、多一欄原本的位置、時間那一欄是刪除時間。
// Ctrl／⌘＋A、Esc、F2 是整個頁面的快捷鍵，由檔案瀏覽畫面處理，再呼叫這裡的方法。

import { h } from "./dom.js";
import { formatSize, formatTime } from "./format.js";
import { fileIconName, folderIconName, iconUrl } from "./file-types.js";
import { icon } from "./icons.js";
import type { SortKey, SortOptions } from "./sort.js";
import type { FileItem } from "./types.js";

export type ViewMode = "list" | "grid";

const VIEW_KEY = "manorcloud.view";

/** 記住的檢視方式（List 或 Grid）：My files 與 Trash 共用同一個設定 */
export function loadView(): ViewMode {
  try {
    return localStorage.getItem(VIEW_KEY) === "grid" ? "grid" : "list";
  } catch {
    return "list";
  }
}

export function saveView(view: ViewMode): void {
  try {
    localStorage.setItem(VIEW_KEY, view);
  } catch {
    // 無法儲存只代表下次回到 List
  }
}

/**
 * 拖曳項目時放在 dataTransfer 中的類型。用自訂的類型區分「站內的項目」與「從電腦拖進來的檔案」：
 * 前者是搬移，後者是上傳
 */
export const DRAG_TYPE = "application/x-manorcloud-items";

interface FileListOptions {
  readonly view: ViewMode;
  /** 雙擊或按 Enter：資料夾進入；圖片預覽；其他檔案下載 */
  readonly onOpen: (item: FileItem) => void;
  /** 選取改變時呼叫：工具列依此啟用或停用按鈕 */
  readonly onSelectionChange: () => void;
  /** 右鍵或鍵盤開啟右鍵選單；座標是視窗中的位置 */
  readonly onContextMenu: (x: number, y: number) => void;
  /** 原位改名按下 Enter。失敗時拋出錯誤，名稱會恢復原狀 */
  readonly onRename: (item: FileItem, name: string) => Promise<void>;
  /** 目前的排序：欄位標題顯示 ↑／↓ */
  readonly sort: SortOptions;
  /** 點欄位標題：依那一欄排序，再點一次反轉方向 */
  readonly onSort: (key: SortKey) => void;
  /** 排序按鈕與選單：List 時放在 Modified 的右邊，Grid 時放在方塊上方 */
  readonly sortControl: HTMLElement;
  /** 可以拖曳搬移、資料夾可以接受放下。垃圾桶中為 false */
  readonly draggable?: boolean;
  /** 有的話，在 Name 右邊多一欄「Original location」，顯示這個函式回傳的文字（垃圾桶） */
  readonly location?: (item: FileItem) => string;
  /** 時間那一欄的名稱，預設 Modified；垃圾桶是 Deleted */
  readonly timeLabel?: string;
  /** 沒有任何項目時顯示的文字 */
  readonly emptyMessage?: string;
  /**
   * 框選的範圍：選取框畫在這個元素裡，座標也以它為準。
   * 是整個內容區而不只是列表，所以從路徑旁、列表下方等任何空白處都能開始框選
   */
  readonly marqueeHost: HTMLElement;
  /**
   * 固定在最前面的資料夾連結（管理員的 users 資料夾）。
   * 只是一個連結：不能選取、拖曳、改名，也不參與排序
   */
  readonly pinned?: readonly PinnedFolder[];
  /** 有縮圖的項目回傳縮圖的網址，取代圖示；載入失敗時改回一般的圖示 */
  readonly thumbnail?: (item: FileItem) => string | null;
}

/** 固定在列表最前面的資料夾連結 */
export interface PinnedFolder {
  readonly name: string;
  readonly href: string;
  /** 顯示在大小那一欄（List）或名稱下方（Grid）的說明，例如人數 */
  readonly note: string;
}

/** 一個項目在畫面上的元素 */
interface ItemNodes {
  readonly item: FileItem;
  /** 表格的一列，或 Grid 的一個方塊 */
  readonly root: HTMLElement;
  readonly box: HTMLInputElement;
  readonly name: HTMLElement;
}

interface Columns {
  readonly name: number;
  readonly size: number;
}

type ColumnKey = keyof Columns;

const COLUMNS_KEY = "manorcloud.columns";
const DEFAULT_COLUMNS: Columns = { name: 420, size: 120 };
const MIN_WIDTH: Columns = { name: 140, size: 70 };
/** Modified 自動佔用剩下的空間，但至少保留這麼寬 */
const MIN_MODIFIED = 150;
const CHECK_COLUMN = 44;
/** 排序按鈕那一欄的寬度：與 CSS 的 .col-sort 相同 */
const SORT_COLUMN = 120;
/** 垃圾桶的 Original location 自動佔用空間，但至少保留這麼寬 */
const MIN_LOCATION = 120;
/** 滑鼠移動超過這個距離才算拖曳，否則是點擊 */
const DRAG_THRESHOLD = 4;
/** 用鍵盤調整欄寬時，每按一次的寬度 */
const RESIZE_STEP = 16;

export class FileList {
  /** 列表的區域：太窄時可以橫向捲動。框選的範圍是更外層的 marqueeHost */
  readonly element: HTMLElement;
  private readonly nodes = new Map<number, ItemNodes>();
  /** 項目的順序，Shift＋點擊與方向鍵依此計算範圍 */
  private readonly order: readonly number[];
  private readonly selected = new Set<number>();
  /** Shift＋點擊的起點：最後一次單獨點擊或切換的項目 */
  private anchor: number | null = null;
  /** 鍵盤焦點所在的項目 */
  private focused: number | null = null;
  private renaming = false;
  /** 拖曳框選剛結束：接下來的那一次 click 不是使用者的點擊 */
  private suppressClick = false;
  /** 正在拖曳搬移的項目；沒有在拖曳時為 null */
  private dragging: ReadonlySet<number> | null = null;
  private selectAllBox: HTMLInputElement | null = null;
  private readonly headers = new Map<ColumnKey, HTMLElement>();
  private columns: Columns = loadColumns();

  constructor(
    private readonly items: readonly FileItem[],
    private readonly options: FileListOptions,
  ) {
    this.order = items.map((item) => item.id);
    this.element = h("div", { class: "file-area" });
    this.element.addEventListener("keydown", (event) => this.handleKey(event));

    if (items.length === 0 && (options.pinned ?? []).length === 0) {
      this.element.append(
        h(
          "p",
          { class: "empty-state" },
          options.emptyMessage ?? "This folder is empty. Drop files here or use Upload to add some.",
        ),
      );
    } else if (options.view === "grid") {
      this.element.append(this.grid());
    } else {
      this.element.append(this.table());
      this.applyColumns();
    }

    // 用 Tab 進入列表時，焦點落在第一個項目
    const first = this.order[0];
    if (first !== undefined) {
      this.nodes.get(first)?.root.setAttribute("tabindex", "0");
    }
  }

  // ==================== 對外的方法 ====================

  /** 目前選取的項目，依列表中的順序 */
  selectedItems(): FileItem[] {
    return this.items.filter((item) => this.selected.has(item.id));
  }

  /** 選取指定的項目（切換檢視方式後還原選取） */
  select(ids: Iterable<number>): void {
    this.selected.clear();
    for (const id of ids) {
      if (this.nodes.has(id)) {
        this.selected.add(id);
      }
    }
    this.update();
  }

  selectAll(): void {
    this.select(this.order);
  }

  clear(): void {
    this.anchor = null;
    this.select([]);
  }

  isRenaming(): boolean {
    return this.renaming;
  }

  /** 正在拖曳搬移的項目。拖曳途中瀏覽器不讓讀取 dataTransfer 的內容，所以由這裡提供 */
  draggingIds(): ReadonlySet<number> | null {
    return this.dragging;
  }

  /**
   * 在原位改名。沒有指定項目時，對唯一選取的那一個改名（F2、工具列）。
   * 打開時選取不含副檔名的部分：直接打字不會不小心刪掉 .jpg
   */
  startRename(target?: FileItem): void {
    const item = target ?? this.onlySelected();
    if (item === undefined || this.renaming) {
      return;
    }
    const node = this.nodes.get(item.id);
    if (node === undefined) {
      return;
    }
    this.selectOnly(item.id);
    this.renaming = true;

    const input = h("input", {
      type: "text",
      class: "rename-input",
      value: item.name,
      "aria-label": `New name for ${item.name}`,
      spellcheck: "false",
      autocomplete: "off",
    });
    node.name.replaceWith(input);
    input.focus();
    const dot = item.name.lastIndexOf(".");
    input.setSelectionRange(0, item.kind === "folder" || dot <= 0 ? item.name.length : dot);

    let finished = false;
    const finish = async (save: boolean): Promise<void> => {
      if (finished) {
        return;
      }
      finished = true;
      const value = input.value.trim();
      if (save && value !== "" && value !== item.name) {
        input.disabled = true;
        try {
          await this.options.onRename(item, value);
          // 成功：檔案瀏覽畫面會重新載入整個列表
          return;
        } catch {
          // 失敗：錯誤訊息由檔案瀏覽畫面顯示，這裡恢復原本的名稱
        }
      }
      input.replaceWith(node.name);
      this.renaming = false;
      node.root.focus();
    };

    input.addEventListener("keydown", (event) => {
      // 不讓 Enter、Esc 等按鍵再傳到列表或頁面的快捷鍵
      event.stopPropagation();
      if (event.key === "Enter") {
        event.preventDefault();
        void finish(true);
      } else if (event.key === "Escape") {
        event.preventDefault();
        void finish(false);
      }
    });
    input.addEventListener("blur", () => void finish(true));
  }

  // ==================== 建立畫面 ====================

  private table(): HTMLElement {
    const box = h("input", { type: "checkbox", class: "select-box", "aria-label": "Select all" });
    box.addEventListener("change", () => (box.checked ? this.selectAll() : this.clear()));
    this.selectAllBox = box;

    const name = h(
      "th",
      { scope: "col", class: "col-name", "aria-sort": this.ariaSort("name") },
      this.sortButton("name", "Name"),
      this.resizer("name", "Name"),
    );
    const size = h(
      "th",
      { scope: "col", class: "col-size num", "aria-sort": this.ariaSort("size") },
      this.sortButton("size", "Size"),
      this.resizer("size", "Size"),
    );
    this.headers.set("name", name);
    this.headers.set("size", size);

    return h(
      "table",
      { class: "files", role: "grid", "aria-multiselectable": "true" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", { scope: "col", class: "col-check" }, box),
          name,
          this.options.location === undefined
            ? null
            : h("th", { scope: "col", class: "col-location" }, "Original location"),
          size,
          h(
            "th",
            { scope: "col", class: "num", "aria-sort": this.ariaSort("modified") },
            this.sortButton("modified", this.options.timeLabel ?? "Modified"),
          ),
          h(
            "th",
            { scope: "col", class: "col-sort" },
            this.options.sortControl,
          ),
        ),
      ),
      h(
        "tbody",
        {},
        (this.options.pinned ?? []).map((pinned) => this.pinnedRow(pinned)),
        this.items.map((item) => this.row(item)),
      ),
    );
  }

  /** 固定的資料夾連結（List）。勾選框與時間的位置留空，欄位才會對齊 */
  private pinnedRow(pinned: PinnedFolder): HTMLElement {
    return h(
      "tr",
      { class: "is-folder pinned" },
      h("td", { class: "col-check" }),
      h(
        "td",
        {},
        h(
          "a",
          { href: pinned.href, class: "name-cell pinned-link" },
          fileIcon(folderIconName(pinned.name), 18),
          h("span", { class: "item-name", title: pinned.name }, pinned.name),
        ),
      ),
      this.options.location === undefined ? null : h("td", { class: "col-location" }),
      h("td", { class: "num muted" }, pinned.note),
      h("td", { class: "num" }),
      h("td", { class: "col-sort" }),
    );
  }

  /** 固定的資料夾連結（Grid） */
  private pinnedTile(pinned: PinnedFolder): HTMLElement {
    return h(
      "li",
      { class: "tile is-folder pinned", role: "none" },
      h(
        "a",
        { href: pinned.href, class: "pinned-link" },
        h("span", { class: "tile-icon" }, fileIcon(folderIconName(pinned.name), 48)),
        h("span", { class: "item-name", title: pinned.name }, pinned.name),
        h("span", { class: "tile-meta" }, pinned.note),
      ),
    );
  }

  /** 欄位標題：點了依這一欄排序。目前排序的那一欄顯示方向的箭頭 */
  private sortButton(key: SortKey, label: string): HTMLButtonElement {
    const { sort } = this.options;
    const active = sort.key === key;
    return h(
      "button",
      {
        type: "button",
        class: active ? "sort-button active" : "sort-button",
        title: `Sort by ${label.toLowerCase()}`,
        onClick: () => this.options.onSort(key),
      },
      label,
      active ? icon(sort.direction === "asc" ? "arrowUp" : "arrowDown", 14) : null,
    );
  }

  /** 螢幕閱讀器用的排序狀態：只有目前排序的那一欄有 */
  private ariaSort(key: SortKey): string | null {
    const { sort } = this.options;
    if (sort.key !== key) {
      return null;
    }
    return sort.direction === "asc" ? "ascending" : "descending";
  }

  private row(item: FileItem): HTMLElement {
    const box = this.checkbox(item);
    const name = h("span", { class: "item-name", title: item.name }, item.name);
    const drop = this.dropTarget(item);
    // 只有圖示與檔名可以拖曳搬移；列的其他地方留給框選
    const handle = h(
      "span",
      { class: "name-cell", draggable: this.canDrag() ? "true" : null },
      this.itemIcon(item, 18),
      name,
    );
    const location = this.options.location?.(item);
    const row = h(
      "tr",
      {
        class: item.kind === "folder" ? "is-folder" : null,
        "data-id": item.id,
        "data-drop-folder": drop?.id ?? null,
        "data-drop-name": drop?.name ?? null,
        "aria-selected": "false",
        tabindex: "-1",
      },
      h("td", { class: "col-check" }, box),
      h("td", {}, handle),
      location === undefined
        ? null
        : h("td", { class: "col-location", title: location }, location),
      h("td", { class: "num" }, formatSize(item.size)),
      h("td", { class: "num" }, formatTime(item.updated_at)),
      h("td", { class: "col-sort" }),
    );
    this.bind(item, row, box, name, handle);
    return row;
  }

  private grid(): HTMLElement {
    return h(
      "div",
      {},
      // Grid 沒有標題列：排序按鈕放在方塊上方的右邊，位置與 List 時相同
      h("div", { class: "grid-header" }, this.options.sortControl),
      h(
        "ul",
        { class: "tiles", role: "listbox", "aria-label": "Files", "aria-multiselectable": "true" },
        (this.options.pinned ?? []).map((pinned) => this.pinnedTile(pinned)),
        this.items.map((item) => this.tile(item)),
      ),
    );
  }

  private tile(item: FileItem): HTMLElement {
    const box = this.checkbox(item);
    const name = h("span", { class: "item-name", title: item.name }, item.name);
    const isFolder = item.kind === "folder";
    const drop = this.dropTarget(item);
    const tile = h(
      "li",
      {
        class: isFolder ? "tile is-folder" : "tile",
        role: "option",
        "data-id": item.id,
        "data-drop-folder": drop?.id ?? null,
        "data-drop-name": drop?.name ?? null,
        "aria-selected": "false",
        draggable: this.canDrag() ? "true" : null,
        tabindex: "-1",
      },
      box,
      h("span", { class: "tile-icon" }, this.itemIcon(item, 48)),
      name,
      h("span", { class: "tile-meta" }, isFolder ? "Folder" : formatSize(item.size)),
    );
    this.bind(item, tile, box, name, tile);
    return tile;
  }

  /** 項目的圖示，或圖片的縮圖 */
  private itemIcon(item: FileItem, size: number): Node {
    const plain = (): Node => fileIcon(item.kind === "folder" ? folderIconName(item.name) : fileIconName(item.name), size);
    const src = this.options.thumbnail?.(item) ?? null;
    if (src === null) {
      return plain();
    }
    const image = h("img", {
      class: size > 24 ? "thumb large" : "thumb",
      src,
      alt: "",
      loading: "lazy",
      decoding: "async",
      draggable: "false",
    });
    image.addEventListener("error", () => image.replaceWith(plain()), { once: true });
    return image;
  }

  private canDrag(): boolean {
    return this.options.draggable ?? true;
  }

  /** 可以接受放下的資料夾才有：拖曳啟用時的資料夾 */
  private dropTarget(item: FileItem): { id: number; name: string } | null {
    return this.canDrag() && item.kind === "folder" ? { id: item.id, name: item.name } : null;
  }

  private checkbox(item: FileItem): HTMLInputElement {
    return h("input", {
      type: "checkbox",
      class: "select-box",
      "aria-label": `Select ${item.name}`,
    });
  }

  /**
   * 項目的滑鼠操作：點擊選取、雙擊開啟、右鍵選單、勾選框，以及從 handle 開始的拖曳搬移。
   * handle 在 List 是圖示與檔名，在 Grid 是整個方塊
   */
  private bind(
    item: FileItem,
    root: HTMLElement,
    box: HTMLInputElement,
    name: HTMLElement,
    handle: HTMLElement,
  ): void {
    root.addEventListener("click", (event) => this.handleClick(event, item));
    root.addEventListener("dblclick", (event) => {
      if (!isControl(event.target) && !this.renaming) {
        this.options.onOpen(item);
      }
    });
    root.addEventListener("contextmenu", (event) => {
      if (this.renaming) {
        return;
      }
      event.preventDefault();
      // 在已選取的項目上按右鍵：保留選取，選單作用在全部；否則改成只選這一個
      if (!this.selected.has(item.id)) {
        this.selectOnly(item.id);
      }
      this.focus(item.id);
      this.options.onContextMenu(event.clientX, event.clientY);
    });

    handle.addEventListener("dragstart", (event) => this.startDrag(event, item));
    handle.addEventListener("dragend", () => {
      this.dragging = null;
    });

    // 勾選框：只切換自己，其他項目維持原狀（相當於 Ctrl＋點擊）
    box.addEventListener("click", (event) => event.stopPropagation());
    box.addEventListener("change", () => {
      this.setSelected(item.id, box.checked);
      this.anchor = item.id;
      this.update();
    });

    this.nodes.set(item.id, { item, root, box, name });
  }

  // ==================== 選取 ====================

  private handleClick(event: MouseEvent, item: FileItem): void {
    if (this.suppressClick || this.renaming || isControl(event.target)) {
      return;
    }
    const additive = event.metaKey || event.ctrlKey;
    if (event.shiftKey && this.anchor !== null) {
      this.selectRange(this.anchor, item.id, additive);
    } else if (additive) {
      this.setSelected(item.id, !this.selected.has(item.id));
      this.anchor = item.id;
      this.update();
    } else {
      this.selectOnly(item.id);
    }
    this.focus(item.id);
  }

  private selectOnly(id: number): void {
    this.anchor = id;
    this.select([id]);
  }

  /** 選取 from 到 to 之間的全部。additive 為 true 時加入原本的選取，否則取代 */
  private selectRange(from: number, to: number, additive: boolean): void {
    const a = this.order.indexOf(from);
    const b = this.order.indexOf(to);
    if (a < 0 || b < 0) {
      return;
    }
    const range = this.order.slice(Math.min(a, b), Math.max(a, b) + 1);
    this.select(additive ? [...this.selected, ...range] : range);
  }

  private setSelected(id: number, on: boolean): void {
    if (on) {
      this.selected.add(id);
    } else {
      this.selected.delete(id);
    }
  }

  private onlySelected(): FileItem | undefined {
    const items = this.selectedItems();
    return items.length === 1 ? items[0] : undefined;
  }

  /** 把選取的狀態反映到畫面上，並通知檔案瀏覽畫面 */
  private update(): void {
    for (const [id, node] of this.nodes) {
      const on = this.selected.has(id);
      node.root.classList.toggle("selected", on);
      node.root.setAttribute("aria-selected", String(on));
      node.box.checked = on;
    }
    if (this.selectAllBox !== null) {
      const count = this.selected.size;
      this.selectAllBox.checked = count > 0 && count === this.order.length;
      this.selectAllBox.indeterminate = count > 0 && count < this.order.length;
    }
    this.options.onSelectionChange();
  }

  /** 鍵盤焦點移到某個項目：只有它可以用 Tab 進入，其他項目用方向鍵移動 */
  private focus(id: number): void {
    if (this.focused !== null && this.focused !== id) {
      this.nodes.get(this.focused)?.root.setAttribute("tabindex", "-1");
    }
    for (const node of this.nodes.values()) {
      if (node.item.id !== id) {
        node.root.setAttribute("tabindex", "-1");
      }
    }
    this.focused = id;
    const node = this.nodes.get(id);
    node?.root.setAttribute("tabindex", "0");
    node?.root.focus({ preventScroll: false });
  }

  // ==================== 鍵盤 ====================

  private handleKey(event: KeyboardEvent): void {
    if (this.renaming || isEditable(event.target)) {
      return;
    }
    const current = this.focused ?? this.order[0];
    if (current === undefined) {
      return;
    }

    const step = this.step(event.key);
    if (step !== 0) {
      event.preventDefault();
      const index = this.order.indexOf(current);
      const next = this.order[Math.max(0, Math.min(this.order.length - 1, index + step))];
      if (next === undefined) {
        return;
      }
      if (event.shiftKey) {
        this.selectRange(this.anchor ?? current, next, event.metaKey || event.ctrlKey);
      } else if (!(event.metaKey || event.ctrlKey)) {
        this.selectOnly(next);
      }
      this.focus(next);
      return;
    }

    const node = this.nodes.get(current);
    if (node === undefined) {
      return;
    }
    if (event.key === " ") {
      event.preventDefault();
      this.setSelected(current, !this.selected.has(current));
      this.anchor = current;
      this.update();
    } else if (event.key === "Enter") {
      event.preventDefault();
      this.options.onOpen(node.item);
    } else if (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey)) {
      event.preventDefault();
      if (!this.selected.has(current)) {
        this.selectOnly(current);
      }
      const rect = node.root.getBoundingClientRect();
      this.options.onContextMenu(rect.left + 24, rect.top + rect.height / 2);
    }
  }

  /** 方向鍵要移動幾個項目：List 上下各一個；Grid 左右一個、上下一整排 */
  private step(key: string): number {
    const grid = this.options.view === "grid";
    switch (key) {
      case "ArrowDown":
        return grid ? this.perRow() : 1;
      case "ArrowUp":
        return grid ? -this.perRow() : -1;
      case "ArrowRight":
        return grid ? 1 : 0;
      case "ArrowLeft":
        return grid ? -1 : 0;
      case "Home":
        return -this.order.length;
      case "End":
        return this.order.length;
      default:
        return 0;
    }
  }

  /** Grid 一排有幾個方塊：與第一個方塊在同一高度的數量 */
  private perRow(): number {
    const tops = Array.from(this.nodes.values(), (node) => node.root.offsetTop);
    const first = tops[0];
    return first === undefined ? 1 : Math.max(1, tops.filter((top) => top === first).length);
  }

  // ==================== 拖曳搬移 ====================

  /**
   * 從項目上開始拖曳：拖的是目前選取的全部項目；拖的那一個沒有被選取時，改成只選它。
   * 游標旁顯示名稱（一個）或數量（多個）
   */
  private startDrag(event: DragEvent, item: FileItem): void {
    const transfer = event.dataTransfer;
    if (this.renaming || transfer === null) {
      event.preventDefault();
      return;
    }
    if (!this.selected.has(item.id)) {
      this.selectOnly(item.id);
    }
    const ids = [...this.selected];
    this.dragging = new Set(ids);
    transfer.effectAllowed = "move";
    transfer.setData(DRAG_TYPE, ids.join(","));

    const label = ids.length === 1 ? item.name : `${ids.length} items`;
    const ghost = h("div", { class: "drag-ghost" }, label);
    document.body.append(ghost);
    transfer.setDragImage(ghost, 14, 14);
    // 瀏覽器在這個事件結束時擷取拖曳圖片，之後就可以移除
    setTimeout(() => ghost.remove());
  }

  // ==================== 拖曳框選 ====================

  /**
   * 按住左鍵拖曳，拉出選取框。可以從列表的空白處開始，也可以從一列中「不是圖示與檔名」的地方開始；
   * 從圖示、檔名或 Grid 的方塊開始的拖曳是搬移（瀏覽器原生的拖放），不在這裡處理。
   * 沒有拖曳就放開：在項目上視為點擊（由 handleClick 處理），在空白處則取消選取。
   * 從按鈕、勾選框、改名欄位、欄寬把手或標題列開始時不觸發。
   * Ctrl／⌘ 按著開始時，框到的項目加入原本的選取，否則取代。
   */
  startMarquee(event: MouseEvent): void {
    if (event.button !== 0 || this.renaming) {
      return;
    }
    const target = event.target;
    if (
      target instanceof Element &&
      target.closest('[draggable="true"], input, button, a, [data-resize], thead, dialog, .menu') !==
        null
    ) {
      return;
    }

    const area = this.options.marqueeHost;
    const additive = event.metaKey || event.ctrlKey;
    const base = additive ? new Set(this.selected) : new Set<number>();
    const startedOnItem = target instanceof Element && target.closest("[data-id]") !== null;
    const origin = this.pointIn(event);
    let box: HTMLElement | null = null;
    const controller = new AbortController();

    const move = (moveEvent: MouseEvent): void => {
      const point = this.pointIn(moveEvent);
      if (box === null) {
        if (Math.hypot(point.x - origin.x, point.y - origin.y) < DRAG_THRESHOLD) {
          return;
        }
        box = h("div", { class: "marquee", "aria-hidden": "true" });
        area.append(box);
      }
      const left = Math.min(origin.x, point.x);
      const top = Math.min(origin.y, point.y);
      const width = Math.abs(point.x - origin.x);
      const height = Math.abs(point.y - origin.y);
      box.style.left = `${left}px`;
      box.style.top = `${top}px`;
      box.style.width = `${width}px`;
      box.style.height = `${height}px`;

      const areaRect = area.getBoundingClientRect();
      const hits = new Set(base);
      for (const [id, node] of this.nodes) {
        const rect = node.root.getBoundingClientRect();
        const rectLeft = rect.left - areaRect.left;
        const rectTop = rect.top - areaRect.top;
        if (
          rectLeft < left + width &&
          rectLeft + rect.width > left &&
          rectTop < top + height &&
          rectTop + rect.height > top
        ) {
          hits.add(id);
        }
      }
      this.select(hits);
    };

    const up = (): void => {
      controller.abort();
      if (box !== null) {
        box.remove();
        // 放開的地方若在某個項目上，瀏覽器接著會送出 click：那不是使用者的點擊
        this.suppressClick = true;
        setTimeout(() => {
          this.suppressClick = false;
        });
      } else if (!startedOnItem && !additive) {
        // 點列表的空白處：取消選取
        this.clear();
      }
    };

    window.addEventListener("mousemove", move, { signal: controller.signal });
    window.addEventListener("mouseup", up, { signal: controller.signal });
    // 不讓拖曳變成選取文字
    event.preventDefault();
  }

  /** 滑鼠在框選範圍中的位置。範圍本身不捲動（捲動的是整個頁面），所以不必加上捲動的距離 */
  private pointIn(event: MouseEvent): { x: number; y: number } {
    const rect = this.options.marqueeHost.getBoundingClientRect();
    return { x: event.clientX - rect.left, y: event.clientY - rect.top };
  }

  // ==================== 欄寬 ====================

  /** 標題儲存格右緣的把手：拖曳或用左右鍵調整這一欄的寬度 */
  private resizer(column: ColumnKey, label: string): HTMLElement {
    const handle = h("span", {
      class: "col-resizer",
      "data-resize": column,
      role: "separator",
      "aria-orientation": "vertical",
      "aria-label": `Resize the ${label} column`,
      title: "Drag to resize",
      tabindex: "0",
    });

    handle.addEventListener("mousedown", (event) => {
      if (event.button !== 0) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      const startX = event.clientX;
      const start = this.columns[column];
      const controller = new AbortController();
      document.body.classList.add("resizing");

      window.addEventListener(
        "mousemove",
        (moveEvent) => this.setColumn(column, start + moveEvent.clientX - startX),
        { signal: controller.signal },
      );
      window.addEventListener(
        "mouseup",
        () => {
          controller.abort();
          document.body.classList.remove("resizing");
          saveColumns(this.columns);
        },
        { signal: controller.signal },
      );
    });

    handle.addEventListener("keydown", (event) => {
      if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
        event.preventDefault();
        event.stopPropagation();
        const step = event.key === "ArrowRight" ? RESIZE_STEP : -RESIZE_STEP;
        this.setColumn(column, this.columns[column] + step);
        saveColumns(this.columns);
      }
    });

    return handle;
  }

  /** 設定一欄的寬度：不小於下限，也不讓 Modified 被擠到小於 MIN_MODIFIED */
  private setColumn(column: ColumnKey, width: number): void {
    const other = column === "name" ? this.columns.size : this.columns.name;
    const max = Math.max(
      MIN_WIDTH[column],
      this.element.clientWidth -
        CHECK_COLUMN -
        other -
        MIN_MODIFIED -
        SORT_COLUMN -
        (this.options.location === undefined ? 0 : MIN_LOCATION),
    );
    const value = Math.round(Math.max(MIN_WIDTH[column], Math.min(max, width)));
    this.columns = { ...this.columns, [column]: value };
    this.applyColumns();
  }

  /** 寬度設定在標題儲存格上：固定版面的表格依第一列決定每一欄的寬度 */
  private applyColumns(): void {
    for (const [column, header] of this.headers) {
      header.style.width = `${this.columns[column]}px`;
    }
  }
}

// ==================== 輔助函式 ====================

/** 事件來自項目中的控制項（勾選框、按鈕、連結、改名欄位）：不當成點擊項目本身 */
function isControl(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest("input, button, a, [data-resize]") !== null;
}

function isEditable(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    (target.isContentEditable || target.matches("input:not([type='checkbox']), textarea, select"))
  );
}

/** 讀取記住的欄寬；格式不對或無法使用 localStorage 時用預設值 */
function loadColumns(): Columns {
  try {
    const raw = localStorage.getItem(COLUMNS_KEY);
    if (raw !== null) {
      const value: unknown = JSON.parse(raw);
      if (
        typeof value === "object" &&
        value !== null &&
        "name" in value &&
        "size" in value &&
        typeof value.name === "number" &&
        typeof value.size === "number"
      ) {
        return {
          name: Math.max(MIN_WIDTH.name, value.name),
          size: Math.max(MIN_WIDTH.size, value.size),
        };
      }
    }
  } catch {
    // 私密瀏覽等情況下 localStorage 可能無法使用：用預設值即可
  }
  return DEFAULT_COLUMNS;
}

function saveColumns(columns: Columns): void {
  try {
    localStorage.setItem(COLUMNS_KEY, JSON.stringify(columns));
  } catch {
    // 無法儲存只代表下次回到預設寬度
  }
}

/** 檔案或資料夾的圖示（Material Icon Theme 的 SVG，見 file-types.ts） */
function fileIcon(name: string, size: number): HTMLImageElement {
  return h("img", {
    class: "file-icon",
    src: iconUrl(name),
    alt: "",
    width: size,
    height: size,
    draggable: "false",
  });
}
