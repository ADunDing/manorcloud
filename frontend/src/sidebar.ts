// 左側欄：New 選單、My files、Trash，以及目前這顆硬碟的容量。
//
// 管理員在 My files 與 Trash 之間多了另外每一顆硬碟（Disk2 等）：
// 點進去是他在那一顆硬碟上的檔案，用法與 My files 相同（見 space.ts）。

import { api } from "./api.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { icon } from "./icons.js";
import { dropdown, menuItem, menuSeparator } from "./menu.js";
import { folderHref, TRASH_HREF, type Disk } from "./router.js";
import { spaceOf, withSpace, type Space } from "./space.js";
import type { StorageInfo, StorageOverview } from "./types.js";

/** 目前所在的區塊：側欄中對應的項目會反白。帳號設定、使用者管理等頁面不屬於任何一個 */
export type Section = "files" | "trash" | null;

export interface SidebarActions {
  readonly newFolder: () => void;
  readonly upload: () => void;
}

/** 使用量超過這個比例時，容量條變成紅色 */
const ALMOST_FULL = 0.9;

/** 容量顯示的元素；側欄沒有顯示時為 null */
let storage: { meter: HTMLElement; fill: HTMLElement; text: HTMLElement } | null = null;
/** 容量顯示的是哪一個檔案空間所在的硬碟 */
let storageSpace: Space = null;

export interface SidebarOptions {
  /** 管理員：My files 與 Trash 之間列出另外每一顆硬碟 */
  readonly admin: boolean;
  /** 目前在哪一顆硬碟的檔案中：undefined 是 My files */
  readonly disk?: Disk;
  /** 正在瀏覽這位使用者的檔案：他的檔案所在的那一項反白 */
  readonly browsing?: number;
  /** 目前的使用者：判斷哪一顆硬碟是 My files */
  readonly userId?: number;
}
/** 載入序號：較慢回來的舊結果不能蓋掉新的 */
let loading = 0;

/**
 * 顯示側欄。
 *
 * @param section 目前所在的區塊。New 只在 My files 可以使用（新增的東西要放進目前的資料夾）
 */
export function renderSidebar(
  container: HTMLElement,
  actions: SidebarActions,
  section: Section,
  options: SidebarOptions = { admin: false },
): void {
  const onFiles = section === "files";
  // My files 只在自己的檔案中反白；在另一顆硬碟上時反白那一顆（硬碟清單讀回來之後）
  const onMyFiles = onFiles && options.disk === undefined;
  const newButton = h(
    "button",
    {
      type: "button",
      class: "new-button",
      disabled: !onFiles,
      title: onFiles ? undefined : "Open My files to add files or folders",
    },
    icon("plus", 22),
    "New",
  );
  const newMenu = h(
    "div",
    { class: "menu dropdown-menu", role: "menu", "aria-label": "New" },
    menuItem({ label: "New folder", icon: "newFolder", onSelect: actions.newFolder }),
    menuSeparator(),
    menuItem({ label: "Upload files", icon: "upload", onSelect: actions.upload }),
  );
  dropdown(newButton, newMenu);

  const meterFill = h("div", { class: "meter-fill" });
  const meter = h(
    "div",
    {
      class: "meter",
      role: "meter",
      "aria-label": "Disk space used",
      "aria-valuemin": "0",
      "aria-valuemax": "100",
      "aria-valuenow": "0",
    },
    meterFill,
  );
  const text = h("p", { class: "storage-text" }, "—");
  storage = { meter, fill: meterFill, text };
  storageSpace = options.disk === undefined ? null : spaceOf(options.disk);

  const myFiles = h(
    "a",
    {
      href: folderHref(null),
      class: "nav-link",
      "aria-current": onMyFiles ? "page" : null,
    },
    icon("folder", 20),
    "My files",
  );
  const myFilesItem = h("li", {}, myFiles);

  container.replaceChildren(
    h("div", { class: "new-wrap" }, newButton, newMenu),
    h(
      "nav",
      { "aria-label": "Sections" },
      h(
        "ul",
        { class: "nav-list" },
        myFilesItem,
        h(
          "li",
          {},
          h(
            "a",
            {
              href: TRASH_HREF,
              class: "nav-link",
              "aria-current": section === "trash" ? "page" : null,
            },
            icon("trash", 20),
            "Trash",
          ),
        ),
      ),
    ),
    h(
      "section",
      { class: "storage", "aria-label": "Storage" },
      h("div", { class: "storage-title" }, icon("disk", 20), "Storage"),
      meter,
      text,
    ),
  );
  container.hidden = false;
  if (options.admin) {
    void loadDisks(myFilesItem, myFiles, section, options);
  }
  refreshStorage();
}

/**
 * 管理員：My files 之後的另外每一顆硬碟（自己的檔案所在的那一顆就是 My files，不重複列出）。
 * 取得失敗就不顯示：只是捷徑，不值得打擾使用者
 */
async function loadDisks(
  myFilesItem: HTMLElement,
  myFiles: HTMLElement,
  section: Section,
  options: SidebarOptions,
): Promise<void> {
  try {
    const overview = await api.get<StorageOverview>("/api/admin/storage");
    const home = overview.users.find((user) => user.user_id === options.userId)?.volume_id ?? null;
    // 反白哪一顆：正在瀏覽某位使用者時，是他的檔案所在的那一顆
    // users 頁面沒有 section，但有 disk：一樣反白那一顆
    let current: Disk | "none" =
      section === "files" || options.disk !== undefined ? options.disk : "none";
    if (options.browsing !== undefined) {
      const owner = overview.users.find((user) => user.user_id === options.browsing);
      // volume_id 是 null 代表資料目錄，不能用 ?? 判斷有沒有找到
      current = owner === undefined ? "none" : owner.volume_id;
    }
    // 自己的檔案所在的那一顆就是 My files
    if (current === home) {
      current = undefined;
    }
    if (current === undefined) {
      myFiles.setAttribute("aria-current", "page");
    }
    const items = overview.volumes
      .filter((volume) => volume.id !== home)
      .map((volume) =>
        h(
          "li",
          {},
          h(
            "a",
            {
              href: folderHref(null, volume.id),
              class: "nav-link",
              "aria-current": current === volume.id ? "page" : null,
            },
            icon("disk", 20),
            volume.name,
          ),
        ),
      );
    myFilesItem.after(...items);
  } catch {
    // 只是捷徑，不值得打擾使用者
  }
}

/** 隱藏側欄（登入畫面、首次登入的修改密碼畫面） */
export function hideSidebar(container: HTMLElement): void {
  container.hidden = true;
  container.replaceChildren();
  storage = null;
}

/**
 * 重新讀取容量。上傳完成、刪除檔案、切換資料夾時呼叫。
 * 取得失敗就維持原本的顯示：這只是輔助資訊，不值得打擾使用者。
 */
export function refreshStorage(): void {
  const token = ++loading;
  void (async () => {
    try {
      const info = await api.get<StorageInfo>(withSpace("/api/storage", storageSpace));
      if (token !== loading || storage === null) {
        return;
      }
      const used = Math.max(0, info.total_bytes - info.available_bytes);
      const fraction = info.total_bytes > 0 ? used / info.total_bytes : 0;
      storage.fill.style.width = `${(fraction * 100).toFixed(1)}%`;
      storage.meter.classList.toggle("almost-full", fraction >= ALMOST_FULL);
      storage.meter.setAttribute("aria-valuenow", String(Math.round(fraction * 100)));
      storage.text.textContent = `${formatSize(used)} of ${formatSize(info.total_bytes)} used`;
    } catch {
      // 維持原本的顯示
    }
  })();
}
