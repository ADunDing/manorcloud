// 網址路由：使用 # 之後的部分，不會送到伺服器，後端不需要處理前端路由。
//
//   #/            我的檔案（根目錄）
//   #/folder/12   資料夾 12
//   #/admin       使用者管理（只有管理員）
//
// 書籤、重新整理、瀏覽器的上一頁都能正常運作。

export type Route =
  | { readonly view: "files"; readonly folderId: number | null }
  | { readonly view: "admin" };

/** 解析目前的網址。無法辨識的網址一律視為根目錄 */
export function currentRoute(): Route {
  const hash = window.location.hash;

  if (hash === "#/admin") {
    return { view: "admin" };
  }

  const match = /^#\/folder\/(\d+)$/.exec(hash);
  const digits = match?.[1];
  if (digits === undefined) {
    return { view: "files", folderId: null };
  }
  const id = Number(digits);
  // 超出安全整數範圍的數字無法精確表示，視為無效
  return { view: "files", folderId: Number.isSafeInteger(id) ? id : null };
}

/** 資料夾的連結。null 代表根目錄 */
export function folderHref(folderId: number | null): string {
  return folderId === null ? "#/" : `#/folder/${folderId}`;
}

/** 使用者管理頁的連結 */
export const ADMIN_HREF = "#/admin";
