// 檔案的 API 以網址參數 space= 區分是哪一組檔案（見後端的 space.rs）：
// 沒有參數是自己的 My files；default 或儲存位置的編號是管理員看到的一顆硬碟。
// 用網址參數而不是 header：下載是瀏覽器直接開啟的連結，無法加上 header。

import type { Location } from "./router.js";

/** null：自己的檔案（My files）；其他：space= 的值 */
export type Space = string | null;

/** 儲存位置（null 是資料目錄）對應的 space= */
function spaceOf(volumeId: number | null): string {
  return volumeId === null ? "default" : String(volumeId);
}

export function spaceOfLocation(location: Location): Space {
  switch (location.kind) {
    case "me":
      return null;
    case "disk":
      return spaceOf(location.disk);
  }
}

/** 在 API 的網址加上 space 參數。自己的檔案不加 */
export function withSpace(path: string, space: Space): string {
  if (space === null) {
    return path;
  }
  return `${path}${path.includes("?") ? "&" : "?"}space=${encodeURIComponent(space)}`;
}

/** 檔案的 API 網址：/api/files?path=相片%2F2026&space=… */
export function apiUrl(base: string, space: Space, params: Record<string, string> = {}): string {
  const query = new URLSearchParams(params).toString();
  return withSpace(query === "" ? base : `${base}?${query}`, space);
}

/** 路徑 → API 用的文字：相片/2026 */
export function joinPath(path: readonly string[], name?: string): string {
  return (name === undefined ? path : [...path, name]).join("/");
}
