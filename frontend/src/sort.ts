// 檔案列表的排序：依名稱、修改時間或大小，升冪或降冪，資料夾在上方或與檔案混合。
//
// 在瀏覽器中排序：伺服器一次回傳整個資料夾的內容，不需要另外向伺服器請求。
// 使用者的選擇記在瀏覽器中，下次打開維持一樣的排序。

import { t } from "./i18n.js";
import type { FileItem } from "./types.js";

export type SortKey = "name" | "modified" | "size";
type SortDirection = "asc" | "desc";

export interface SortOptions {
  readonly key: SortKey;
  readonly direction: SortDirection;
  /** 資料夾一律排在檔案上方（Google Drive 的預設） */
  readonly foldersFirst: boolean;
}

const DEFAULT_SORT: SortOptions = { key: "name", direction: "asc", foldersFirst: true };

const STORAGE_KEY = "manorcloud.sort";

/**
 * 名稱的比較：數字依大小排（file2 在 file10 前面），不分大小寫。
 * 不指定語系：依使用者瀏覽器的語言，中文與英文都有合理的順序
 */
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

/** 回傳排序後的新陣列，不改動原本的 */
export function sortItems(items: readonly FileItem[], options: SortOptions): FileItem[] {
  const sign = options.direction === "asc" ? 1 : -1;
  return [...items].sort((a, b) => {
    if (options.foldersFirst && a.kind !== b.kind) {
      return a.kind === "folder" ? -1 : 1;
    }
    // 主要條件相同時依名稱，再依編號：每次排出來的順序都一樣
    return (
      compare(a, b, options.key) * sign || collator.compare(a.name, b.name) || a.id - b.id
    );
  });
}

function compare(a: FileItem, b: FileItem, key: SortKey): number {
  switch (key) {
    case "name":
      return collator.compare(a.name, b.name);
    case "modified":
      return a.updated_at - b.updated_at;
    case "size":
      // 資料夾沒有大小，視為最小
      return (a.size ?? -1) - (b.size ?? -1);
  }
}

/** 換到另一個欄位時的方向：名稱 A→Z；時間與大小由新到舊、由大到小（最常用的看法） */
export function defaultDirection(key: SortKey): SortDirection {
  return key === "name" ? "asc" : "desc";
}

/** 選單中「方向」的文字，依欄位而不同 */
export function directionLabel(key: SortKey, direction: SortDirection): string {
  const labels: Record<SortKey, [string, string]> = {
    name: [t("A to Z"), t("Z to A")],
    modified: [t("Oldest first"), t("Newest first")],
    size: [t("Smallest first"), t("Largest first")],
  };
  const [ascending, descending] = labels[key];
  return direction === "asc" ? ascending : descending;
}

/**
 * 讀取記住的排序。My files 與 Trash 各自記住（storageKey 不同），
 * 垃圾桶預設以刪除時間由新到舊排列
 */
export function loadSort(storageKey = STORAGE_KEY, fallback = DEFAULT_SORT): SortOptions {
  try {
    const raw = localStorage.getItem(storageKey);
    if (raw !== null) {
      const value: unknown = JSON.parse(raw);
      if (
        typeof value === "object" &&
        value !== null &&
        "key" in value &&
        "direction" in value &&
        "foldersFirst" in value &&
        (value.key === "name" || value.key === "modified" || value.key === "size") &&
        (value.direction === "asc" || value.direction === "desc") &&
        typeof value.foldersFirst === "boolean"
      ) {
        return { key: value.key, direction: value.direction, foldersFirst: value.foldersFirst };
      }
    }
  } catch {
    // 無法使用 localStorage 或格式不對：用預設值
  }
  return fallback;
}

export function saveSort(options: SortOptions, storageKey = STORAGE_KEY): void {
  try {
    localStorage.setItem(storageKey, JSON.stringify(options));
  } catch {
    // 無法儲存只代表下次回到預設
  }
}
