// 左側欄：New 選單、My files、Trash，以及檔案所在硬碟的容量。

import { api } from "./api.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { icon } from "./icons.js";
import { dropdown, menuItem, menuSeparator } from "./menu.js";
import { folderHref, TRASH_HREF } from "./router.js";
import type { StorageInfo } from "./types.js";

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
): void {
  const onFiles = section === "files";
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

  container.replaceChildren(
    h("div", { class: "new-wrap" }, newButton, newMenu),
    h(
      "nav",
      { "aria-label": "Sections" },
      h(
        "ul",
        { class: "nav-list" },
        h(
          "li",
          {},
          h(
            "a",
            {
              href: folderHref(null),
              class: "nav-link",
              "aria-current": onFiles ? "page" : null,
            },
            icon("folder", 20),
            "My files",
          ),
        ),
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
  refreshStorage();
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
      const info = await api.get<StorageInfo>("/api/storage");
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
