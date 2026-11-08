// 所有 API 呼叫的唯一入口：JSON 請求與解析、錯誤處理、401 與「必須修改密碼」的通知都只寫一次。
// 上傳用的 XHR（upload.ts）不經過 request，但同樣透過 errorFromResponse 建立錯誤並觸發通知。

import type { ErrorCode } from "./types.js";

/**
 * API 回傳錯誤時拋出的例外。
 * - status 為 0 代表連不到伺服器
 * - code 是給程式判斷的代碼；axum 內建的錯誤（純文字回應）沒有代碼，為 null
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: ErrorCode | null;

  constructor(status: number, message: string, code: ErrorCode | null = null) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }
}

export interface RequestOptions {
  /**
   * 收到 401 時是否通知「登入失效」，預設為 true。
   * 登入 API 的 401 代表「帳號或密碼錯誤」而不是「登入失效」，呼叫時要設為 false，
   * 否則登入失敗會被當成「需要重新登入」。
   */
  notifyUnauthorized?: boolean;
}

type Listener = () => void;

const unauthorizedListeners = new Set<Listener>();
const passwordChangeListeners = new Set<Listener>();

/** 註冊「登入失效」的處理函式（例如切換到登入畫面） */
export function onUnauthorized(listener: Listener): void {
  unauthorizedListeners.add(listener);
}

/**
 * 註冊「必須先修改密碼」的處理函式（例如切換到修改畫面）。
 * 任何 API 回 403 且代碼為 password_change_required 時觸發。
 */
export function onPasswordChangeRequired(listener: Listener): void {
  passwordChangeListeners.add(listener);
}

/**
 * 從錯誤回應建立 ApiError，並觸發對應的全域通知。
 * fetch 與 XHR 都經過這裡，兩者對 401 與「必須修改密碼」的處理才會一致。
 */
export function errorFromResponse(
  status: number,
  body: unknown,
  notifyUnauthorized = true,
): ApiError {
  const { message, code } = parseError(status, body);
  if (status === 401 && notifyUnauthorized) {
    unauthorizedListeners.forEach((listener) => listener());
  }
  if (code === "password_change_required") {
    passwordChangeListeners.forEach((listener) => listener());
  }
  return new ApiError(status, message, code);
}

/**
 * 這個錯誤是否已由全域處理換了畫面（401 → 登入畫面；password_change_required → 修改畫面）。
 * 是的話，呼叫端不該再把錯誤畫上去，否則會覆蓋掉剛出現的畫面。
 */
export function isHandledGlobally(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    (error.status === 401 || error.code === "password_change_required")
  );
}

async function request<T>(
  method: string,
  path: string,
  json: unknown,
  { notifyUnauthorized = true }: RequestOptions = {},
): Promise<T> {
  const headers: Record<string, string> = {};
  const init: RequestInit = {
    method,
    // 同源請求預設就會帶 Cookie，這裡明確寫出來，意圖更清楚
    credentials: "same-origin",
    headers,
  };
  if (json !== undefined) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(json);
  }

  let response: Response;
  try {
    response = await fetch(path, init);
  } catch {
    // fetch 只有在網路層失敗時才會拋出（伺服器沒開、斷線），HTTP 錯誤不會
    throw new ApiError(0, "無法連線到伺服器");
  }

  const body = await readBody(response);

  if (!response.ok) {
    throw errorFromResponse(response.status, body, notifyUnauthorized);
  }

  // 型別由呼叫端指定，相信伺服器的回應符合 types.ts 中的定義
  return body as T;
}

/** 依 Content-Type 讀取回應：JSON 解析成物件，其他當成文字，204 沒有內容 */
async function readBody(response: Response): Promise<unknown> {
  if (response.status === 204) {
    return null;
  }
  const type = response.headers.get("Content-Type") ?? "";
  if (type.includes("application/json")) {
    try {
      return await response.json();
    } catch {
      return null;
    }
  }
  return await response.text();
}

/**
 * 從錯誤回應中取出訊息與代碼。
 * 我們自己的錯誤是 {"error": "...", "code": "..."}；axum 內建的錯誤（400、415、422）目前是純文字，
 * 兩種都要能處理，直到後端統一格式為止。
 */
function parseError(
  status: number,
  body: unknown,
): { message: string; code: ErrorCode | null } {
  if (typeof body === "object" && body !== null) {
    const message =
      "error" in body && typeof body.error === "string"
        ? body.error
        : `請求失敗（${status}）`;
    // 代碼的型別相信伺服器；未知的代碼仍會原樣保留，判斷時比對不到而已
    const code =
      "code" in body && typeof body.code === "string"
        ? (body.code as ErrorCode)
        : null;
    return { message, code };
  }
  if (typeof body === "string" && body.trim() !== "") {
    return { message: body.trim(), code: null };
  }
  return { message: `請求失敗（${status}）`, code: null };
}

export const api = {
  get: <T>(path: string, options?: RequestOptions) =>
    request<T>("GET", path, undefined, options),

  post: <T>(path: string, json?: unknown, options?: RequestOptions) =>
    request<T>("POST", path, json, options),

  patch: <T>(path: string, json?: unknown, options?: RequestOptions) =>
    request<T>("PATCH", path, json, options),

  delete: <T = null>(path: string, options?: RequestOptions) =>
    request<T>("DELETE", path, undefined, options),
};
