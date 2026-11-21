// 網址（hash）與畫面的對應。資料夾以路徑表示，每一段各自編碼：
//
//   #/files/相片/2026        My files 中的資料夾（#/ 是 My files 的最上層）
//   #/trash                  垃圾桶
//   #/disk/{disk}/files/…    管理員：一顆硬碟的最上層（disk 是儲存位置的編號，資料目錄是 default）
//   #/disk/{disk}/trash      那一顆硬碟上的垃圾桶
//   #/settings/{section}     個人設定：account、sign-in、appearance
//   #/admin/{section}        管理（只有管理員）：users、storage、sign-in
//
// 不認得的網址一律回到 My files；不認得的區塊回到該頁的第一個區塊。

export type SettingsSection = "account" | "sign-in" | "appearance";
export type AdminSection = "users" | "storage" | "sign-in";

/** 一組檔案在哪裡：自己的 My files，或管理員看到的一顆硬碟 */
export type Location =
  | { readonly kind: "me" }
  | { readonly kind: "disk"; readonly disk: number | null };

export const MY_FILES: Location = { kind: "me" };

type Route =
  | { readonly view: "files"; readonly location: Location; readonly path: readonly string[] }
  | { readonly view: "trash"; readonly location: Location }
  | { readonly view: "shared" }
  | { readonly view: "settings"; readonly section: SettingsSection }
  | { readonly view: "admin"; readonly section: AdminSection };

const SETTINGS_SECTIONS: readonly SettingsSection[] = ["account", "sign-in", "appearance"];
const ADMIN_SECTIONS: readonly AdminSection[] = ["users", "storage", "sign-in"];

export const TRASH_HREF = "#/trash";
export const SHARED_HREF = "#/shared";

export function currentRoute(): Route {
  return parseRoute(window.location.hash);
}

function parseRoute(hash: string): Route {
  const raw = hash.replace(/^#/, "");

  // 位數上限：比 Number.MAX_SAFE_INTEGER 短，轉換後一定是精確的整數
  const disk = /^\/disk\/(default|\d{1,15})(\/.*)?$/.exec(raw);
  if (disk?.[1] !== undefined) {
    const id = disk[1] === "default" ? null : Number(disk[1]);
    if (id === null || id > 0) {
      const location: Location = { kind: "disk", disk: id };
      const rest = disk[2] ?? "/";
      if (rest === "/trash") {
        return { view: "trash", location };
      }
      const files = /^\/files((?:\/[^/]*)*)$/.exec(rest);
      const path = parsePath(files?.[1] ?? "");
      if (path !== null) {
        return { view: "files", location, path };
      }
    }
  }

  if (raw === "/shared") {
    return { view: "shared" };
  }

  if (raw === "/trash") {
    return { view: "trash", location: MY_FILES };
  }

  const files = /^\/files((?:\/[^/]*)*)$/.exec(raw);
  if (files !== null) {
    const path = parsePath(files[1] ?? "");
    if (path !== null) {
      return { view: "files", location: MY_FILES, path };
    }
  }

  const settings = /^\/settings(?:\/([a-z-]+))?\/?$/.exec(raw);
  if (settings !== null) {
    return { view: "settings", section: pick(SETTINGS_SECTIONS, settings[1]) ?? "account" };
  }

  const admin = /^\/admin(?:\/([a-z-]+))?\/?$/.exec(raw);
  if (admin !== null) {
    return { view: "admin", section: pick(ADMIN_SECTIONS, admin[1]) ?? "users" };
  }

  return { view: "files", location: MY_FILES, path: [] };
}

/** "/相片/2026" → ["相片", "2026"]。編碼錯誤或含有 . 與 .. 時回傳 null */
function parsePath(raw: string): string[] | null {
  const parts: string[] = [];
  for (const segment of raw.split("/").filter((part) => part !== "")) {
    let name: string;
    try {
      name = decodeURIComponent(segment);
    } catch {
      return null;
    }
    if (name === "." || name === ".." || name.includes("/")) {
      return null;
    }
    parts.push(name);
  }
  return parts;
}

/** value 是清單中的其中一個時回傳它（型別也跟著確定），否則 undefined */
function pick<T extends string>(options: readonly T[], value: string | undefined): T | undefined {
  return options.find((option) => option === value);
}

function encodePath(path: readonly string[]): string {
  return path.map((part) => `/${encodeURIComponent(part)}`).join("");
}

/** 一個資料夾 */
export function filesHref(location: Location, path: readonly string[] = []): string {
  switch (location.kind) {
    case "me":
      return path.length === 0 ? "#/" : `#/files${encodePath(path)}`;
    case "disk":
      return `#/disk/${location.disk ?? "default"}/files${encodePath(path)}`;
  }
}

/** 垃圾桶 */
export function trashHref(location: Location = MY_FILES): string {
  return location.kind === "disk" ? `#/disk/${location.disk ?? "default"}/trash` : TRASH_HREF;
}

export function settingsHref(section: SettingsSection = "account"): string {
  return `#/settings/${section}`;
}

export function adminHref(section: AdminSection = "users"): string {
  return `#/admin/${section}`;
}
