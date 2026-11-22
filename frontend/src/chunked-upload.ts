// 分段上傳（可續傳）：大檔案切成 8 到 64 MiB 的段落（依網路速度調整），同時送出好幾段。
//
// 斷線、休眠、伺服器重啟時自動重試，從伺服器已確認的位置繼續，不必整個重傳。
// 重新整理之後，使用者選擇同一個檔案，也能接著伺服器上未完成的上傳繼續（見 isSameFile）。
// 對外的介面與單次上傳相同（UploadHandle），佇列不需要知道是哪一種。

import { t } from "./i18n.js";
import { api, ApiError, errorFromResponse } from "./api.js";
import { withSpace, type Space } from "./space.js";
import { readBody, UploadCancelled, type UploadHandle } from "./upload.js";

/**
 * 每段的大小範圍。伺服器接受最多 128 MiB 的一段。
 * 每段結束時伺服器都要寫進硬碟並更新紀錄，段落越大，這些等待越少；
 * 但網路慢時一段要傳很久，所以從小的開始，傳得快才加大
 */
const MIN_CHUNK = 8 * 1024 * 1024;
const MAX_CHUNK = 64 * 1024 * 1024;

/** 一段在這麼短的時間內傳完就加倍，超過 SLOW_CHUNK_MS 就減半 */
const FAST_CHUNK_MS = 5_000;
const SLOW_CHUNK_MS = 30_000;

/**
 * 同時送出幾段。瀏覽器單一連線的上傳速度有限（Chrome 約 40 MB/s），
 * 好幾條連線才能用滿網路；一段在等伺服器寫進硬碟時，其他段也還在傳。
 *
 * - HTTP/1.1：瀏覽器對同一個伺服器最多開 6 條連線，留 2 條給頁面的其他請求（列表、縮圖）
 * - HTTP/2、HTTP/3：所有請求共用同一條連線，沒有 6 條的限制
 */
const PARALLEL_HTTP1 = 4;
const PARALLEL_MULTIPLEXED = 8;

/**
 * 最多跑在伺服器已確認長度前面多遠：前面那段慢了，後面的不能無限制地往前送。
 * 足夠讓 PARALLEL_MULTIPLEXED 段同時在傳；伺服器的上限是 512 MiB，必須比它小
 */
const MAX_AHEAD = 7 * MAX_CHUNK;

/** 這個頁面與伺服器之間用的協定：「http/1.1」、「h2」、「h3」，瀏覽器不提供時是空字串 */
function protocol(): string {
  const [navigation] = performance.getEntriesByType("navigation");
  return navigation instanceof PerformanceNavigationTiming ? navigation.nextHopProtocol : "";
}

/** 依協定決定同時送幾段。不知道協定時當作 HTTP/1.1：保守一點，不會佔滿連線 */
function parallelChunks(): number {
  const used = protocol();
  return used === "h2" || used === "h3" ? PARALLEL_MULTIPLEXED : PARALLEL_HTTP1;
}

/** 超過這個大小才使用分段上傳；更小的檔案分段沒有好處，只多了請求數 */
export const CHUNKED_THRESHOLD = MIN_CHUNK;

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
  /** 正在送出的請求（同時好幾段） */
  private readonly requests = new Set<XMLHttpRequest>();
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
    for (const xhr of this.requests) {
      xhr.abort();
    }
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

  track(xhr: XMLHttpRequest): void {
    this.requests.add(xhr);
  }

  untrack(xhr: XMLHttpRequest): void {
    this.requests.delete(xhr);
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
    return t("The server is still finishing the previous attempt. Retrying in {seconds} s…", { seconds });
  }
  return error instanceof ApiError && error.status !== 0
    ? t("Connection lost ({status}). Retrying in {seconds} s…", { status: error.status, seconds })
    : t("Connection lost. Retrying in {seconds} s…", { seconds });
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

/**
 * 送出每一段，直到伺服器確認收到全部。
 *
 * 同時最多送 parallelChunks() 段。任何一段失敗時不再送新的，等送出中的都結束，
 * 再向伺服器查詢已確認的位置，從那裡重新開始（已經傳過的後面幾段可能會重送）。
 */
async function sendChunks(
  file: File,
  info: UploadInfo,
  { onProgress, onNotice }: ChunkedUploadOptions,
  control: Control,
): Promise<void> {
  /** 伺服器已確認的長度：從頭開始連續收到多少 */
  let received = info.received;
  let failures = 0;
  let chunkSize = MIN_CHUNK;
  const parallel = parallelChunks();
  // 需要先向伺服器查詢已確認的位置（重試之前、位置不一致時）
  let needsSync = false;

  for (;;) {
    control.check();
    if (needsSync) {
      try {
        const status = await api.get<UploadInfo>(
          control.url(`/api/uploads/${encodeURIComponent(info.id)}`),
        );
        received = status.received;
        needsSync = false;
      } catch (error) {
        control.check();
        if (!isRetryable(error) || failures >= MAX_FAILURES) {
          throw failures > 0 ? interrupted(error) : error;
        }
        const delay = retryDelay(failures);
        failures += 1;
        onNotice(retryNotice(error, delay));
        await control.sleep(delay);
        continue;
      }
    }
    onProgress(received / file.size);
    if (received >= file.size) {
      return;
    }

    // 這一輪從已確認的位置開始。進度 = 起點 + 已傳完的段落 + 送出中各段已送出的部分
    const base = received;
    let next = received;
    let finished = 0;
    const loaded = new Map<number, number>();
    const showProgress = (): void => {
      let sending = 0;
      for (const bytes of loaded.values()) {
        sending += bytes;
      }
      onProgress(Math.min(base + finished + sending, file.size) / file.size);
    };

    let failure: unknown = null;
    const inFlight = new Set<Promise<void>>();
    for (;;) {
      while (
        failure === null &&
        inFlight.size < parallel &&
        next < file.size &&
        next - received < MAX_AHEAD
      ) {
        const start = next;
        const end = Math.min(start + chunkSize, file.size);
        next = end;
        const began = performance.now();
        const onLoaded = (bytes: number): void => {
          loaded.set(start, bytes);
          showProgress();
        };
        const sent: Promise<void> = putChunk(info.id, start, file.slice(start, end), control, onLoaded)
          .then((confirmed) => {
            received = Math.max(received, confirmed);
            finished += end - start;
            const took = performance.now() - began;
            if (took < FAST_CHUNK_MS) {
              chunkSize = Math.min(chunkSize * 2, MAX_CHUNK);
            } else if (took > SLOW_CHUNK_MS) {
              chunkSize = Math.max(chunkSize / 2, MIN_CHUNK);
            }
            failures = 0;
            onNotice(null);
          })
          .catch((error: unknown) => {
            failure ??= error;
          })
          .finally(() => {
            loaded.delete(start);
            inFlight.delete(sent);
            showProgress();
          });
        inFlight.add(sent);
      }
      if (inFlight.size === 0) {
        break;
      }
      await Promise.race(inFlight);
    }

    if (failure === null) {
      // 全部送出了，伺服器卻還沒確認到最後（例如伺服器重新啟動過，忘了後面已收到的段落）：
      // 問一次正確的位置，從那裡補送
      if (received < file.size) {
        needsSync = true;
      }
      continue;
    }

    // 取消造成的中斷：直接停下
    control.check();
    const error = failure;
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

/** 完成上傳，建立檔案。同名時伺服器預設自動改名為「名稱 (1)」，不會拒絕 */
async function complete(
  info: UploadInfo,
  onNotice: (message: string | null) => void,
  control: Control,
): Promise<void> {
  control.check();
  control.completing = true;
  onNotice(t("Finishing…"));

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
      t("Connection lost. The upload is paused: choose the same file again to continue where it stopped"),
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
    const done = (): void => control.untrack(xhr);

    xhr.upload.addEventListener("progress", (event) => {
      if (event.lengthComputable) {
        onLoaded(event.loaded);
      }
    });
    xhr.addEventListener("load", () => {
      done();
      const body = readBody(xhr);
      if (xhr.status >= 200 && xhr.status < 300) {
        const received = (body as { received?: unknown } | null)?.received;
        if (typeof received === "number") {
          resolve(received);
        } else {
          reject(new ApiError(xhr.status, t("The server sent an unexpected response")));
        }
      } else {
        // 與 fetch 一樣觸發 401 與「必須修改密碼」的全域處理
        reject(errorFromResponse(xhr.status, body));
      }
    });
    xhr.addEventListener("error", () => {
      done();
      reject(new ApiError(0, t("Can't reach the server")));
    });
    xhr.addEventListener("timeout", () => {
      done();
      reject(new ApiError(0, t("The connection timed out")));
    });
    xhr.addEventListener("abort", () => {
      done();
      reject(new UploadCancelled());
    });

    xhr.open("PUT", control.url(`/api/uploads/${encodeURIComponent(id)}?offset=${offset}`));
    xhr.timeout = CHUNK_TIMEOUT_MS;
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.send(chunk);
  });
}
