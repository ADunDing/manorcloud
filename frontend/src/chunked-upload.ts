// 分段上傳（可續傳）：大檔案切成 8 MiB 的段落依序送出。
//
// 斷線、休眠、伺服器重啟時自動重試，從伺服器已確認的位置繼續，不必整個重傳。
// 重新整理之後，使用者選擇同一個檔案，也能接著伺服器上未完成的上傳繼續（見 isSameFile）。
// 對外的介面與單次上傳相同（UploadHandle），佇列不需要知道是哪一種。

import { api, ApiError, errorFromResponse } from "./api.js";
import { withSpace, type Space } from "./space.js";
import { readBody, UploadCancelled, type UploadHandle } from "./upload.js";

/** 每段的大小。伺服器接受最多 16 MiB 的一段 */
const CHUNK_SIZE = 8 * 1024 * 1024;

/** 超過這個大小才使用分段上傳；更小的檔案分段沒有好處，只多了請求數 */
export const CHUNKED_THRESHOLD = CHUNK_SIZE;

/** 連續失敗時，第 n 次重試前等待的時間；超過陣列長度後一直用最後一個 */
const RETRY_DELAYS_MS = [1_000, 2_000, 4_000, 8_000, 15_000, 30_000];

/** 連續失敗幾次就放棄（約 5 分鐘）。成功一次就重新計算 */
const MAX_FAILURES = 12;

/** 一段上傳的逾時：卡住的連線不會永遠等下去 */
const CHUNK_TIMEOUT_MS = 5 * 60 * 1000;

/** 伺服器上一筆未完成的上傳 */
export interface UploadInfo {
  id: string;
  /** 上傳到哪個空間（space= 的值） */
  space: string;
  /** 空間中的哪個資料夾，例如 相片/2026 */
  path: string;
  name: string;
  size: number;
  received: number;
  file_modified: number | null;
  updated_at: number;
}

interface ChunkedUploadOptions {
  /** 目標所在的空間（見 space.ts）；沒有時是自己的 My files */
  space?: Space;
  onProgress: (fraction: number) => void;
  /** 顯示重試等提示，null 代表清除 */
  onNotice: (message: string | null) => void;
  /** 得知上傳編號時呼叫：新的上傳在宣告之後，續傳則立刻 */
  onStarted?: (id: string) => void;
  /** 續傳伺服器上已存在的上傳；沒有時宣告新的上傳 */
  resume?: UploadInfo;
}

/**
 * 使用者選的檔案，是不是伺服器上這個未完成的上傳的那一個。
 *
 * 瀏覽器基於安全理由，重新整理後不會記得使用者選過哪個檔案，只能比對重新選的檔案：
 * - 名稱先轉成 NFC 再比：macOS 的檔名常以分解形式出現，伺服器存的是 NFC
 * - 大小必須相同
 * - 修改時間必須與宣告時記錄的相同：同名同大小、內容卻被改過的檔案，
 *   接在舊資料後面會產生內容錯亂的結果。伺服器無法檢查內容，這裡寧可保守
 */
export function isSameFile(file: File, info: UploadInfo): boolean {
  return (
    file.name.normalize("NFC") === info.name &&
    file.size === info.size &&
    info.file_modified !== null &&
    file.lastModified === info.file_modified
  );
}

/** 開始分段上傳 */
export function startChunkedUpload(
  file: File,
  dir: string,
  options: ChunkedUploadOptions,
): UploadHandle {
  const control = new Control(options.space ?? null);
  const done = run(file, dir, options, control);
  return { done, cancel: () => control.cancel() };
}

// ==================== 取消與等待 ====================

/** 一次上傳的控制：取消、可被取消的等待、目前的請求 */
class Control {
  private cancelled = false;
  private xhr: XMLHttpRequest | null = null;
  private wake: (() => void) | null = null;
  /** 已宣告的上傳編號：取消時用來通知伺服器刪除 */
  uploadId: string | null = null;
  /** 完成請求已送出：檔案可能已經建立，不能再取消 */
  completing = false;

  /** @param space 這次上傳所在的檔案空間：每個請求的網址都要帶上 */
  constructor(readonly space: Space) {}

  /** 這次上傳的 API 網址 */
  url(path: string): string {
    return withSpace(path, this.space);
  }

  cancel(): void {
    if (this.cancelled || this.completing) {
      return;
    }
    this.cancelled = true;
    this.xhr?.abort();
    this.wake?.();
    if (this.uploadId !== null) {
      discard(this.uploadId, this.space);
    }
  }

  /** 已取消時拋出 UploadCancelled，讓上傳流程停下來 */
  check(): void {
    if (this.cancelled) {
      throw new UploadCancelled();
    }
  }

  isCancelled(): boolean {
    return this.cancelled;
  }

  track(xhr: XMLHttpRequest | null): void {
    this.xhr = xhr;
  }

  /** 等待一段時間；取消時立刻結束 */
  sleep(ms: number): Promise<void> {
    return new Promise((resolve) => {
      const finish = (): void => {
        clearTimeout(timer);
        this.wake = null;
        resolve();
      };
      const timer = setTimeout(finish, ms);
      this.wake = finish;
    });
  }
}

/** 刪除請求最多嘗試幾次，每次間隔逐漸拉長（總共約 15 秒） */
const DISCARD_ATTEMPTS = 6;

/** 正在向伺服器刪除的上傳。刪除要一點時間，期間這些上傳不該被當成「未完成」 */
const discarding = new Map<string, Promise<void>>();

/**
 * 通知伺服器刪除上傳，立刻釋放空間。
 *
 * 取消的當下，伺服器可能還在處理剛被中斷的那一段（握著檔案鎖），刪除會暫時失敗，
 * 所以隔一下再試。全部失敗也沒關係：24 小時後由垃圾回收刪除。
 */
function discard(id: string, space: Space): void {
  if (discarding.has(id)) {
    return;
  }
  const done = (async () => {
    for (let attempt = 1; attempt <= DISCARD_ATTEMPTS; attempt += 1) {
      try {
        const response = await fetch(withSpace(`/api/uploads/${encodeURIComponent(id)}`, space), {
          method: "DELETE",
        });
        // 已刪除，或早就不存在
        if (response.ok || response.status === 404) {
          return;
        }
      } catch {
        // 連不到伺服器：隔一下再試
      }
      await new Promise((resolve) => setTimeout(resolve, attempt * 1000));
    }
  })().finally(() => discarding.delete(id));
  discarding.set(id, done);
}

/** 這個上傳的刪除結束時完成（已刪除或放棄重試）；沒有在刪除時立刻完成 */
export function whenDiscarded(id: string): Promise<void> {
  return discarding.get(id) ?? Promise.resolve();
}

/** 等待之後可以重試的錯誤：網路或伺服器暫時的問題 */
function isRetryable(error: unknown): boolean {
  if (!(error instanceof ApiError)) {
    return false;
  }
  // 斷線後伺服器可能還握著上一個請求的檔案鎖，幾秒後就會釋放
  if (error.code === "upload_busy") {
    return true;
  }
  return [0, 408, 502, 503, 504].includes(error.status);
}

function retryDelay(failures: number): number {
  // 專案開啟了 noUncheckedIndexedAccess：索引取值的結果可能是 undefined，
  // 即使這裡的計算不會超出範圍，也要提供預設值（與陣列最後一個相同）
  return RETRY_DELAYS_MS[Math.min(failures, RETRY_DELAYS_MS.length - 1)] ?? 30_000;
}

/**
 * 重試前顯示的說明。區分兩種原因，使用者（與除錯時）才知道發生了什麼：
 * - upload_busy：伺服器還在處理上一次被中斷的請求，握著這個上傳的鎖
 * - 其他：連不到伺服器或連線中斷
 */
function retryNotice(error: unknown, delay: number): string {
  const seconds = Math.round(delay / 1000);
  if (error instanceof ApiError && error.code === "upload_busy") {
    return `The server is still finishing the previous attempt. Retrying in ${seconds} s…`;
  }
  const status = error instanceof ApiError && error.status !== 0 ? ` (${error.status})` : "";
  return `Connection lost${status}. Retrying in ${seconds} s…`;
}

// ==================== 流程 ====================

async function run(
  file: File,
  dir: string,
  options: ChunkedUploadOptions,
  control: Control,
): Promise<void> {
  // 1. 宣告（續傳時沿用伺服器上的上傳）
  const info =
    options.resume ??
    (await api.post<UploadInfo>(control.url("/api/uploads"), {
      path: dir,
      name: file.name,
      size: file.size,
      // 重新整理後用來比對是不是同一個檔案
      file_modified: file.lastModified,
    }));
  control.uploadId = info.id;
  options.onStarted?.(info.id);

  // 宣告期間被取消：Control.cancel 那時還不知道編號，由這裡通知伺服器刪除
  if (control.isCancelled()) {
    discard(info.id, control.space);
    throw new UploadCancelled();
  }

  await sendChunks(file, info, options, control);
  await complete(info, options.onNotice, control);
}

/** 依序送出每一段，直到伺服器確認收到全部 */
async function sendChunks(
  file: File,
  info: UploadInfo,
  { onProgress, onNotice }: ChunkedUploadOptions,
  control: Control,
): Promise<void> {
  let offset = info.received;
  let failures = 0;
  // 需要先向伺服器查詢已確認的位置（重試之前、位置不一致時）
  let needsSync = false;

  for (;;) {
    control.check();
    try {
      if (needsSync) {
        const status = await api.get<UploadInfo>(
          control.url(`/api/uploads/${encodeURIComponent(info.id)}`),
        );
        offset = status.received;
        needsSync = false;
      }
      onProgress(offset / file.size);
      if (offset >= file.size) {
        return;
      }

      const start = offset;
      const end = Math.min(start + CHUNK_SIZE, file.size);
      offset = await putChunk(info.id, start, file.slice(start, end), control, (loaded) =>
        onProgress((start + loaded) / file.size),
      );
      failures = 0;
      onNotice(null);
    } catch (error) {
      // 取消造成的中斷：直接停下
      control.check();

      if (error instanceof ApiError && error.code === "offset_mismatch") {
        // 不必等待：問伺服器正確的位置，從那裡繼續。也計入失敗，避免無限循環
        needsSync = true;
        failures += 1;
        if (failures > MAX_FAILURES) {
          throw error;
        }
        continue;
      }
      if (!isRetryable(error) || failures >= MAX_FAILURES) {
        throw failures > 0 ? interrupted(error) : error;
      }

      const delay = retryDelay(failures);
      failures += 1;
      onNotice(retryNotice(error, delay));
      await control.sleep(delay);
      // 斷線前伺服器可能已經收到並確認了一部分
      needsSync = true;
    }
  }
}

/** 完成上傳，建立檔案。同名時伺服器預設自動改名為「名稱 (1)」，不會拒絕 */
async function complete(
  info: UploadInfo,
  onNotice: (message: string | null) => void,
  control: Control,
): Promise<void> {
  control.check();
  control.completing = true;
  onNotice("Finishing…");

  let failures = 0;
  for (;;) {
    try {
      await api.post(control.url(`/api/uploads/${encodeURIComponent(info.id)}/complete`), {});
      onNotice(null);
      return;
    } catch (error) {
      // 上一次的完成請求送出了、回應卻沒收到：重試時上傳已經不在，代表上一次其實成功了
      if (failures > 0 && error instanceof ApiError && error.status === 404) {
        onNotice(null);
        return;
      }
      if (!isRetryable(error) || failures >= MAX_FAILURES) {
        throw failures > 0 ? interrupted(error) : error;
      }

      const delay = retryDelay(failures);
      failures += 1;
      onNotice(retryNotice(error, delay));
      await new Promise((resolve) => setTimeout(resolve, delay));
    }
  }
}

/** 重試多次仍然失敗：說明上傳已經暫停，之後可以續傳 */
function interrupted(error: unknown): unknown {
  if (error instanceof ApiError && error.status === 0) {
    return new ApiError(
      0,
      "Connection lost. The upload is paused: choose the same file again to continue where it stopped",
    );
  }
  return error;
}

// ==================== 一段 ====================

/**
 * 送出一段，回傳伺服器新的已確認長度。
 * 用 XHR 而不是 fetch：需要上傳進度（upload.onprogress）。
 */
function putChunk(
  id: string,
  offset: number,
  chunk: Blob,
  control: Control,
  onLoaded: (loaded: number) => void,
): Promise<number> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    control.track(xhr);

    xhr.upload.addEventListener("progress", (event) => {
      if (event.lengthComputable) {
        onLoaded(event.loaded);
      }
    });
    xhr.addEventListener("load", () => {
      control.track(null);
      const body = readBody(xhr);
      if (xhr.status >= 200 && xhr.status < 300) {
        const received = (body as { received?: unknown } | null)?.received;
        if (typeof received === "number") {
          resolve(received);
        } else {
          reject(new ApiError(xhr.status, "The server sent an unexpected response"));
        }
      } else {
        // 與 fetch 一樣觸發 401 與「必須修改密碼」的全域處理
        reject(errorFromResponse(xhr.status, body));
      }
    });
    xhr.addEventListener("error", () => {
      control.track(null);
      reject(new ApiError(0, "Can't reach the server"));
    });
    xhr.addEventListener("timeout", () => {
      control.track(null);
      reject(new ApiError(0, "The connection timed out"));
    });
    xhr.addEventListener("abort", () => {
      control.track(null);
      reject(new UploadCancelled());
    });

    xhr.open("PUT", control.url(`/api/uploads/${encodeURIComponent(id)}?offset=${offset}`));
    xhr.timeout = CHUNK_TIMEOUT_MS;
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.send(chunk);
  });
}
