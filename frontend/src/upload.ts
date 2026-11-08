// 單一檔案的上傳（一次送出整個檔案），用於小檔案。大檔案見 chunked-upload.ts。
//
// 用 XMLHttpRequest 而不是 fetch：fetch 無法回報「已經上傳了多少」，
// XHR 有 upload.onprogress。XHR 同樣是同源請求，會自動帶上 Cookie 與 Origin，CSRF 防護照常運作。

import { ApiError, errorFromResponse } from "./api.js";

/** 使用者取消了上傳 */
export class UploadCancelled extends Error {
  constructor() {
    super("已取消");
    this.name = "UploadCancelled";
  }
}

/** 一個進行中的上傳。單次上傳與分段上傳都實作這個介面，佇列不需要知道是哪一種 */
export interface UploadHandle {
  /** 上傳完成時結束；失敗時拋出 ApiError 或 UploadCancelled */
  readonly done: Promise<void>;
  /** 中斷上傳 */
  cancel(): void;
}

/**
 * 開始上傳一個檔案。
 *
 * 檔名放在網址參數 name，而不是只依賴 multipart 的檔名：
 * 瀏覽器編碼 multipart 檔名時會把 " 變成 %22，卻不編碼 % 本身，伺服器無法還原。
 * URLSearchParams 的編碼規則與後端的解碼規則一致，檔名能一字不差地送達。
 */
export function startUpload(
  file: File,
  folderId: number | null,
  onProgress: (fraction: number) => void,
): UploadHandle {
  const params = new URLSearchParams({ name: file.name });
  if (folderId !== null) {
    params.set("parent_id", String(folderId));
  }

  const xhr = new XMLHttpRequest();

  const done = new Promise<void>((resolve, reject) => {
    xhr.upload.addEventListener("progress", (event) => {
      if (event.lengthComputable && event.total > 0) {
        onProgress(event.loaded / event.total);
      }
    });

    xhr.addEventListener("load", () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve();
      } else {
        // 與 fetch 一樣觸發 401 與「必須修改密碼」的全域處理
        reject(errorFromResponse(xhr.status, readBody(xhr)));
      }
    });

    xhr.addEventListener("error", () =>
      reject(new ApiError(0, "無法連線到伺服器")),
    );
    // 伺服器端的 TempBlob 守衛會刪除寫到一半的暫存檔
    xhr.addEventListener("abort", () => reject(new UploadCancelled()));
  });

  // 欄位名稱必須是 file，而且是第一個欄位（後端的要求）
  const form = new FormData();
  form.append("file", file);

  xhr.open("POST", `/api/files?${params.toString()}`);
  xhr.send(form);

  return { done, cancel: () => xhr.abort() };
}

/** 依 Content-Type 讀取 XHR 的回應：JSON 解析成物件，其他當成文字 */
export function readBody(xhr: XMLHttpRequest): unknown {
  const type = xhr.getResponseHeader("Content-Type") ?? "";
  if (type.includes("application/json")) {
    try {
      return JSON.parse(xhr.responseText) as unknown;
    } catch {
      return null;
    }
  }
  return xhr.responseText;
}
