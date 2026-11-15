// 網址（hash）與畫面的對應：
//
//   #/                     My files
//   #/folder/{id}          某個資料夾
//   #/trash                垃圾桶
//   #/settings/{section}   個人設定：account、sign-in、appearance
//   #/admin/{section}      管理（只有管理員）：users、sign-in
//
// 不認得的網址一律回到 My files；不認得的區塊回到該頁的第一個區塊。
// 之後加入 Storage 等區塊時，只要加進對應的清單。

export type SettingsSection = "account" | "sign-in" | "appearance";
export type AdminSection = "users" | "sign-in";

export type Route =
  | { readonly view: "files"; readonly folderId: number | null }
  | { readonly view: "trash" }
  | { readonly view: "settings"; readonly section: SettingsSection }
  | { readonly view: "admin"; readonly section: AdminSection };

export const SETTINGS_SECTIONS: readonly SettingsSection[] = ["account", "sign-in", "appearance"];
export const ADMIN_SECTIONS: readonly AdminSection[] = ["users", "sign-in"];

export const TRASH_HREF = "#/trash";

export function currentRoute(): Route {
  return parseRoute(window.location.hash);
}

export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, "");

  if (path === "/trash") {
    return { view: "trash" };
  }

  // 位數上限：比 Number.MAX_SAFE_INTEGER 短，轉換後一定是精確的整數
  const folder = /^\/folder\/(\d{1,15})$/.exec(path)?.[1];
  if (folder !== undefined) {
    const id = Number(folder);
    if (id > 0) {
      return { view: "files", folderId: id };
    }
  }

  const settings = /^\/settings(?:\/([a-z-]+))?\/?$/.exec(path);
  if (settings !== null) {
    return { view: "settings", section: pick(SETTINGS_SECTIONS, settings[1]) ?? "account" };
  }

  const admin = /^\/admin(?:\/([a-z-]+))?\/?$/.exec(path);
  if (admin !== null) {
    return { view: "admin", section: pick(ADMIN_SECTIONS, admin[1]) ?? "users" };
  }

  return { view: "files", folderId: null };
}

/** value 是清單中的其中一個時回傳它（型別也跟著確定），否則 undefined */
function pick<T extends string>(options: readonly T[], value: string | undefined): T | undefined {
  return options.find((option) => option === value);
}

export function folderHref(folderId: number | null): string {
  return folderId === null ? "#/" : `#/folder/${folderId}`;
}

export function settingsHref(section: SettingsSection = "account"): string {
  return `#/settings/${section}`;
}

export function adminHref(section: AdminSection = "users"): string {
  return `#/admin/${section}`;
}
