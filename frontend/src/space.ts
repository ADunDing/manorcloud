// 管理員在各硬碟上的檔案空間（側邊欄的 Disk2 等）。
//
// 後端以網址參數 space= 區分：沒有參數是自己的檔案（My files），
// space=default 或 space={儲存位置的編號} 是管理員在那一顆硬碟上的檔案。
// 用網址參數而不是 header：下載是瀏覽器直接開啟的連結，無法加上 header。

/** null：自己的檔案（My files）；"default" 或位置的編號：那一顆硬碟上的檔案空間 */
export type Space = string | null;

/** 儲存位置（null 是資料目錄）對應的檔案空間 */
export function spaceOf(volumeId: number | null): string {
  return volumeId === null ? "default" : String(volumeId);
}

/** 在 API 的網址加上 space 參數。自己的檔案不加，網址與原本完全相同 */
export function withSpace(path: string, space: Space): string {
  if (space === null) {
    return path;
  }
  return `${path}${path.includes("?") ? "&" : "?"}space=${encodeURIComponent(space)}`;
}
