// 左側欄：New 選單、My files、Trash，以及目前這顆硬碟的容量。
//
// 管理員在 My files 與 Trash 之間多了另外每一顆硬碟（Disk2 等）：
// 點進去是那一顆硬碟的最上層，用法與 My files 相同（見 space.ts）。

import { t } from "./i18n.js";
import { api } from "./api.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { icon } from "./icons.js";
import { dropdown, menuItem, menuSeparator } from "./menu.js";
import { filesHref, MY_FILES, SHARED_HREF, TRASH_HREF, type Location } from "./router.js";
import { spaceOfLocation, withSpace, type Space } from "./space.js";
import type { Mount, StorageInfo, StorageOverview } from "./types.js";

/** 目前所在的區塊：側欄中對應的項目會反白。帳號設定、使用者管理等頁面不屬於任何一個 */
type Section = "files" | "trash" | "shared" | null;

interface SidebarActions {
  readonly newFolder: () => void;
  readonly upload: () => void;
  readonly uploadFolder: () => void;
}

/** 使用量超過這個比例時，容量條變成紅色 */
const ALMOST_FULL = 0.9;

/** 容量顯示的元素；側欄沒有顯示時為 null */
let storage: { meter: HTMLElement; fill: HTMLElement; text: HTMLElement } | null = null;
/** 容量顯示的是哪一個檔案空間所在的硬碟 */
let storageSpace: Space = null;

interface SidebarOptions {
  /** 管理員：My files 與 Trash 之間列出另外每一顆硬碟 */
  readonly admin: boolean;
  /** 目前在哪一組檔案中 */
  readonly location?: Location;
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
  const location = options.location ?? MY_FILES;
  const onFiles = section === "files";
  // My files 只在自己的檔案中反白；在另一顆硬碟上時反白那一顆（硬碟清單讀回來之後）
  const onMyFiles = section === "files" && location.kind === "me";
  const newButton = h(
    "button",
    {
      type: "button",
      class: "new-button",
      disabled: !onFiles,
      title: onFiles ? undefined : t("Open My files to add files or folders"),
    },
    icon("plus", 22),
    t("New"),
  );
  const newMenu = h(
    "div",
    { class: "menu dropdown-menu", role: "menu", "aria-label": t("New") },
    menuItem({ label: t("New folder"), icon: "newFolder", onSelect: actions.newFolder }),
    menuSeparator(),
    menuItem({ label: t("Upload files"), icon: "upload", onSelect: actions.upload }),
    menuItem({ label: t("Upload folder"), icon: "upload", onSelect: actions.uploadFolder }),
  );
  dropdown(newButton, newMenu);

  const meterFill = h("div", { class: "meter-fill" });
  const meter = h(
    "div",
    {
      class: "meter",
      role: "meter",
      "aria-label": t("Disk space used"),
      "aria-valuemin": "0",
      "aria-valuemax": "100",
      "aria-valuenow": "0",
    },
    meterFill,
  );
  const text = h("p", { class: "storage-text" }, "—");
  storage = { meter, fill: meterFill, text };
  storageSpace = spaceOfLocation(location);

  const myFiles = h(
    "a",
    {
      href: filesHref(MY_FILES),
      class: "nav-link",
      "aria-current": onMyFiles ? "page" : null,
    },
    icon("folder", 20),
    t("My files"),
  );
  const myFilesItem = h("li", {}, myFiles);
  const trashItem = h(
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
      t("Trash"),
    ),
  );

  container.replaceChildren(
    h("div", { class: "new-wrap" }, newButton, newMenu),
    h(
      "nav",
      { "aria-label": t("Sections") },
      h(
        "ul",
        { class: "nav-list" },
        myFilesItem,
        h(
          "li",
          {},
          h(
            "a",
            { href: SHARED_HREF, class: "nav-link", "aria-current": section === "shared" ? "page" : null },
            icon("link", 20),
            t("Shared links"),
          ),
        ),
        trashItem,
      ),
    ),
    h(
      "section",
      { class: "storage", "aria-label": t("Storage") },
      h("div", { class: "storage-title" }, icon("disk", 20), t("Storage")),
      meter,
      text,
    ),
  );
  container.hidden = false;
  if (options.admin) {
    void loadDisks(myFilesItem, myFiles, section, location, options.userId);
  } else {
    void loadMounts(myFilesItem, section, location);
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
  location: Location,
  userId: number | undefined,
): Promise<void> {
  try {
    const overview = await api.get<StorageOverview>("/api/admin/storage");
    const home = overview.users.find((user) => user.user_id === userId)?.volume_id ?? null;
    const disk = section === "files" && location.kind === "disk" ? location.disk : undefined;
    // 自己的檔案所在的那一顆就是 My files
    if (disk !== undefined && disk === home) {
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
              href: filesHref({ kind: "disk", disk: volume.id }),
              class: "nav-link",
              "aria-current": disk === volume.id ? "page" : null,
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

/**
 * 一般使用者：管理員掛載給他的儲存位置，排在 My files 之後。
 * 取得失敗就不顯示：只是捷徑，不值得打擾使用者
 */
async function loadMounts(myFilesItem: HTMLElement, section: Section, location: Location): Promise<void> {
  try {
    const mounts = await api.get<Mount[]>("/api/mounts");
    const disk = section === "files" && location.kind === "disk" ? location.disk : undefined;
    const items = mounts.map((mount) =>
      h(
        "li",
        {},
        h(
          "a",
          {
            href: filesHref({ kind: "disk", disk: mount.id }),
            class: "nav-link",
            "aria-current": disk === mount.id ? "page" : null,
          },
          icon("disk", 20),
          mount.name,
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
      storage.text.textContent = t("{used} of {total} used", { used: formatSize(used), total: formatSize(info.total_bytes) });
    } catch {
      // 維持原本的顯示
    }
  })();
}
