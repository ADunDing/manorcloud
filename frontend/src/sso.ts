// SSO（單一登入）在前端的共用部分：開始登入或連結、從身分提供者回來後的結果。
//
// 開始時向伺服器要一個登入網址（伺服器同時在 Cookie 中放入這次登入的 state），
// 然後把整個頁面送到身分提供者。登入完成後，身分提供者把瀏覽器帶回伺服器，
// 伺服器再跳轉回 ManorCloud：成功時網址中有 ?sso=linked（連結），失敗時有 ?sso_error=原因。

import { api } from "./api.js";

/** 登入畫面需要的資訊（GET /api/auth/sso） */
interface SsoInfo {
  readonly enabled: boolean;
  readonly display_name: string | null;
}

/** 自己的連結狀態（GET /api/me/sso） */
export interface MySso {
  readonly enabled: boolean;
  readonly display_name: string | null;
  readonly linked: boolean;
  readonly has_password: boolean;
}

/** 管理員的設定（GET／PUT /api/admin/sso）。client secret 本身永遠不會傳回來 */
export interface AdminSso {
  readonly enabled: boolean;
  readonly display_name: string;
  readonly issuer_url: string;
  readonly client_id: string;
  readonly has_secret: boolean;
  readonly public_url: string;
  readonly auto_create: boolean;
  readonly redirect_uri: string | null;
}

/** 登入畫面用：不需要登入就能讀取 */
export function ssoInfo(): Promise<SsoInfo> {
  return api.get<SsoInfo>("/api/auth/sso", { notifyUnauthorized: false });
}

/**
 * 開始登入（login）或把身分連結到目前的帳號（link），然後離開這個頁面前往身分提供者。
 * 開始失敗時拋出錯誤，由呼叫端顯示
 */
export async function startSso(mode: "login" | "link"): Promise<void> {
  const path = mode === "login" ? "/api/auth/sso/start" : "/api/me/sso/link";
  const { url } = await api.post<{ url: string }>(path, undefined, {
    // 登入畫面的 401 不代表「登入已過期」
    notifyUnauthorized: mode === "link",
  });
  window.location.assign(url);
}

/** 從身分提供者回來的結果 */
interface SsoResult {
  /** 失敗的說明；沒有失敗時為 null */
  readonly error: string | null;
  /** 剛完成連結 */
  readonly linked: boolean;
}

/**
 * 讀取網址中的結果，並從網址列移除：重新整理或分享網址時不會再出現一次。
 * 只在程式啟動時呼叫一次
 */
export function takeSsoResult(): SsoResult {
  const params = new URLSearchParams(window.location.search);
  const code = params.get("sso_error");
  const linked = params.get("sso") === "linked";
  if (code !== null || linked) {
    history.replaceState(null, "", window.location.pathname + window.location.hash);
  }
  return { error: code === null ? null : ssoErrorMessage(code), linked };
}

/** 伺服器的失敗代碼 → 給人看的說明（見後端 web/sso.rs 的 Failure） */
function ssoErrorMessage(code: string): string {
  switch (code) {
    case "expired":
      return "The sign-in took too long or was started in another browser. Try again.";
    case "cancelled":
      return "Single sign-on was cancelled.";
    case "no_account":
      return "There's no ManorCloud account for you yet. Ask your administrator to create one.";
    case "disabled":
      return "This account is disabled. Contact your administrator.";
    case "not_configured":
      return "Single sign-on isn't set up.";
    case "identity_taken":
      return "That account is already linked to another ManorCloud user.";
    case "already_linked":
      return "Your account is already linked.";
    case "session_changed":
      return "You were logged out before linking finished. Log in and try again.";
    default:
      return "Single sign-on didn't work. Try again, or contact your administrator.";
  }
}
