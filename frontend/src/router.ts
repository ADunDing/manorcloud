// 網址（hash）與畫面的對應：
//
//   #/                     My files
//   #/folder/{id}          某個資料夾
//   #/trash                垃圾桶
//   #/settings/{section}   個人設定：account、sign-in、appearance
//   #/admin/{section}      管理（只有管理員）：users、storage、sign-in
//   #/disk/{disk}          管理員在另一顆硬碟上的檔案（disk 是儲存位置的編號，資料目錄是 default）
//   #/disk/{disk}/folder/{id}
//   #/disk/{disk}/trash    那一顆硬碟上的垃圾桶
//   #/disk/{disk}/users    放在那一顆硬碟上的使用者（My files 與每顆硬碟第一層的 users 資料夾）
//   #/browse/{user}        管理員瀏覽某位使用者的檔案（唯讀）
//   #/browse/{user}/folder/{id}
//
// 不認得的網址一律回到 My files；不認得的區塊回到該頁的第一個區塊。
// 加入新的區塊時，只要加進對應的清單。

export type SettingsSection = "account" | "sign-in" | "appearance";
export type AdminSection = "users" | "storage" | "sign-in";

/**
 * 管理員的檔案在哪一顆硬碟：undefined 是自己的檔案（My files），
 * null 是資料目錄（Default），數字是儲存位置的編號
 */
export type Disk = number | null | undefined;

export type Route =
  | { readonly view: "files"; readonly folderId: number | null; readonly disk: Disk }
  | { readonly view: "trash"; readonly disk: Disk }
  | { readonly view: "users"; readonly disk: number | null }
  | { readonly view: "settings"; readonly section: SettingsSection }
  | { readonly view: "admin"; readonly section: AdminSection }
  | { readonly view: "browse"; readonly userId: number; readonly folderId: number | null };

export const SETTINGS_SECTIONS: readonly SettingsSection[] = ["account", "sign-in", "appearance"];
export const ADMIN_SECTIONS: readonly AdminSection[] = ["users", "storage", "sign-in"];

export const TRASH_HREF = "#/trash";

export function currentRoute(): Route {
  return parseRoute(window.location.hash);
}

export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, "");

  // 管理員在另一顆硬碟上的檔案：#/disk/{disk}/...，後面的部分與自己的檔案相同
  const onDisk = /^\/disk\/(default|\d{1,15})(\/.*)?$/.exec(path);
  const disk: Disk =
    onDisk?.[1] === undefined ? undefined : onDisk[1] === "default" ? null : Number(onDisk[1]);
  const rest = onDisk === null ? path : (onDisk[2] ?? "/");

  if (rest === "/trash") {
    return { view: "trash", disk };
  }

  if (rest === "/users" && disk !== undefined) {
    return { view: "users", disk };
  }

  // 位數上限：比 Number.MAX_SAFE_INTEGER 短，轉換後一定是精確的整數
  const folder = /^\/folder\/(\d{1,15})$/.exec(rest)?.[1];
  if (folder !== undefined) {
    const id = Number(folder);
    if (id > 0) {
      return { view: "files", folderId: id, disk };
    }
  }

  if (disk !== undefined) {
    return { view: "files", folderId: null, disk };
  }

  const browse = /^\/browse\/(\d{1,15})(?:\/folder\/(\d{1,15}))?$/.exec(path);
  if (browse?.[1] !== undefined) {
    const userId = Number(browse[1]);
    const folderId = browse[2] === undefined ? null : Number(browse[2]);
    if (userId > 0 && (folderId === null || folderId > 0)) {
      return { view: "browse", userId, folderId };
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

  return { view: "files", folderId: null, disk: undefined };
}

/** value 是清單中的其中一個時回傳它（型別也跟著確定），否則 undefined */
function pick<T extends string>(options: readonly T[], value: string | undefined): T | undefined {
  return options.find((option) => option === value);
}

export function folderHref(folderId: number | null, disk: Disk = undefined): string {
  const base = disk === undefined ? "#" : `#/disk/${disk ?? "default"}`;
  return folderId === null ? `${base}/` : `${base}/folder/${folderId}`;
}

export function trashHref(disk: Disk = undefined): string {
  return disk === undefined ? TRASH_HREF : `#/disk/${disk ?? "default"}/trash`;
}

/** 放在這一顆硬碟上的使用者（第一層的 users 資料夾） */
export function usersHref(disk: number | null): string {
  return `#/disk/${disk ?? "default"}/users`;
}

export function settingsHref(section: SettingsSection = "account"): string {
  return `#/settings/${section}`;
}

export function adminHref(section: AdminSection = "users"): string {
  return `#/admin/${section}`;
}

/** 管理員瀏覽某位使用者的檔案 */
export function browseHref(userId: number, folderId: number | null = null): string {
  return folderId === null ? `#/browse/${userId}` : `#/browse/${userId}/folder/${folderId}`;
}
