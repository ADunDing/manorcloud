// 兩步驟驗證（2FA）的設定流程：Settings → Sign-in 與「管理員要求設定」的畫面共用。
//
// 三個步驟：
// 1. 輸入目前的密碼（伺服器產生新的密鑰）
// 2. 用驗證器 App 掃描 QR code（或手動輸入密鑰），輸入 App 顯示的 6 位數確認
// 3. 顯示 10 組備用碼：只顯示這一次，請使用者存起來
//
// QR code 由伺服器算出點陣，這裡用 SVG 畫出來：不使用任何外部的 QR 服務，密鑰不會離開這台伺服器。

import { copyText } from "./clipboard.js";
import { t } from "./i18n.js";
import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";

const SVG_NS = "http://www.w3.org/2000/svg";

/** GET /api/me/two-factor */
export interface TwoFactorStatus {
  readonly enabled: boolean;
  readonly recovery_codes_left: number;
  /** 管理員要求開啟：開啟之後不能自己關閉 */
  readonly required: boolean;
  /** 只有能用密碼登入的帳號可以設定；只用 SSO 的帳號由身分提供者負責 */
  readonly available: boolean;
}

interface SetupResponse {
  readonly secret: string;
  readonly uri: string;
  readonly qr: { readonly size: number; readonly modules: string };
}

interface RecoveryCodes {
  readonly recovery_codes: readonly string[];
}

interface SetupOptions {
  /** 完成（看過備用碼並按下 Done）之後呼叫 */
  readonly onDone: () => void;
  /** 有的話顯示 Cancel；管理員要求設定的畫面沒有 */
  readonly onCancel?: () => void;
}

/** 在 container 中顯示設定流程，從輸入密碼開始 */
export function renderTwoFactorSetup(container: HTMLElement, options: SetupOptions): void {
  const password = h("input", { type: "password", required: true, autocomplete: "current-password" });
  const errorBox = h("p", { class: "error", role: "alert" });
  const next = h("button", { type: "submit", class: "primary" }, t("Continue"));

  const form = h(
    "form",
    { method: "post", class: "auth-form embedded" },
    h("p", {}, t("Enter your password to start. You'll need an authenticator app on your phone, such as Google Authenticator, Microsoft Authenticator or 1Password.")),
    h("label", {}, t("Current password"), password),
    errorBox,
    h("div", { class: "actions" }, next, cancelButton(options)),
  );
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    next.disabled = true;
    errorBox.textContent = "";
    void (async () => {
      try {
        const setup = await api.post<SetupResponse>("/api/me/two-factor/setup", {
          current_password: password.value,
        });
        showScanStep(container, setup, options);
      } catch (error) {
        if (!isHandledGlobally(error)) {
          errorBox.textContent = messageOf(error);
          password.select();
        }
        next.disabled = false;
      }
    })();
  });

  container.replaceChildren(form);
  password.focus();
}

/** 第 2 步：掃描 QR code，輸入驗證碼確認 */
function showScanStep(container: HTMLElement, setup: SetupResponse, options: SetupOptions): void {
  const code = codeInput();
  const errorBox = h("p", { class: "error", role: "alert" });
  const turnOn = h("button", { type: "submit", class: "primary" }, t("Turn on"));
  // 手動輸入時每 4 個字元一組，比較不容易看錯
  const grouped = setup.secret.match(/.{1,4}/g)?.join(" ") ?? setup.secret;

  const form = h(
    "form",
    { method: "post", class: "auth-form embedded" },
    h("p", {}, t("1. Scan this QR code with your authenticator app.")),
    h("div", { class: "qr-box" }, qrSvg(setup.qr.size, setup.qr.modules)),
    h(
      "details",
      { class: "manual-secret" },
      h("summary", {}, t("Can't scan it? Enter this key instead")),
      h("code", {}, grouped),
    ),
    h("p", {}, t("2. Enter the 6-digit code the app shows.")),
    h("label", {}, t("Code"), code),
    errorBox,
    h("div", { class: "actions" }, turnOn, cancelButton(options)),
  );
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    turnOn.disabled = true;
    errorBox.textContent = "";
    void (async () => {
      try {
        const result = await api.post<RecoveryCodes>("/api/me/two-factor/enable", {
          code: code.value,
        });
        showRecoveryCodes(container, result.recovery_codes, options.onDone);
      } catch (error) {
        if (!isHandledGlobally(error)) {
          errorBox.textContent = messageOf(error);
          code.select();
        }
        turnOn.disabled = false;
      }
    })();
  });

  container.replaceChildren(form);
  code.focus();
}

/**
 * 備用碼：只顯示這一次。提供複製與下載，按下 Done 才結束。
 * 也用於「換一組新的備用碼」
 */
export function showRecoveryCodes(
  container: HTMLElement,
  codes: readonly string[],
  onDone: () => void,
): void {
  const text = codes.join("\n");
  const copy = h("button", { type: "button", class: "secondary" }, t("Copy"));
  const download = h("button", { type: "button", class: "secondary" }, t("Download"));
  const done = h("button", { type: "button", class: "primary" }, t("Done"));
  const status = h("p", { class: "hint", role: "status" });

  copy.addEventListener("click", () => {
    void copyText(text, copy).then((copied) => {
      // 瀏覽器不允許複製時：改用下載
      status.textContent = copied ? t("Copied.") : t("Your browser can't copy here. Use Download instead.");
    });
  });
  download.addEventListener("click", () => {
    const file = new Blob([`${t("ManorCloud recovery codes")}\n\n${text}\n`], { type: "text/plain" });
    const url = URL.createObjectURL(file);
    const link = h("a", { href: url, download: "manorcloud-recovery-codes.txt", hidden: true });
    document.body.append(link);
    link.click();
    link.remove();
    URL.revokeObjectURL(url);
  });
  done.addEventListener("click", onDone);

  container.replaceChildren(
    h(
      "div",
      { class: "auth-form embedded" },
      h("p", {}, h("strong", {}, t("Two-factor authentication is on."))),
      h(
        "p",
        {},
        t("Save these recovery codes somewhere safe. If you lose your phone, each code lets you log in once. They won't be shown again."),
      ),
      h("ol", { class: "recovery-codes" }, codes.map((c) => h("li", {}, h("code", {}, c)))),
      h("div", { class: "actions" }, copy, download, done),
      status,
    ),
  );
  done.focus();
}

/** 6 位數驗證碼或備用碼的輸入欄 */
export function codeInput(): HTMLInputElement {
  return h("input", {
    type: "text",
    required: true,
    // 手機顯示數字鍵盤；瀏覽器可以自動填入簡訊或 App 的驗證碼
    inputmode: "numeric",
    autocomplete: "one-time-code",
    autocapitalize: "none",
    spellcheck: "false",
    maxlength: 20,
    class: "code-input",
  });
}

/** QR code：每個黑點是一個 1×1 的方塊，全部畫成同一條路徑；四周留 4 格空白（規格要求） */
function qrSvg(size: number, modules: string): SVGSVGElement {
  const margin = 4;
  const total = size + margin * 2;
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${total} ${total}`);
  svg.setAttribute("class", "qr");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", t("QR code for your authenticator app"));
  svg.setAttribute("shape-rendering", "crispEdges");

  const background = document.createElementNS(SVG_NS, "rect");
  background.setAttribute("width", String(total));
  background.setAttribute("height", String(total));
  background.setAttribute("fill", "#ffffff");

  let d = "";
  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      if (modules[y * size + x] === "1") {
        d += `M${x + margin} ${y + margin}h1v1h-1z`;
      }
    }
  }
  const path = document.createElementNS(SVG_NS, "path");
  path.setAttribute("d", d);
  // 不論深色或淺色主題，QR code 一律白底黑點：有些 App 讀不了反色的 QR code
  path.setAttribute("fill", "#000000");

  svg.append(background, path);
  return svg;
}

function cancelButton(options: SetupOptions): HTMLElement | null {
  const { onCancel } = options;
  if (onCancel === undefined) {
    return null;
  }
  return h("button", { type: "button", class: "secondary", onClick: onCancel }, t("Cancel"));
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : t("Something went wrong");
}
