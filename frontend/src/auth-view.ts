// 登入畫面，以及修改帳號名稱與密碼的畫面。

import { t } from "./i18n.js";
import { api, ApiError } from "./api.js";
import { h } from "./dom.js";
import { ssoInfo, startSso } from "./sso.js";
import { codeInput } from "./two-factor-view.js";
import type { User } from "./types.js";

// 以下規則必須與後端 src/web/auth.rs 的 USERNAME_MIN、USERNAME_MAX、
// PASSWORD_MIN、PASSWORD_MAX 及 validate_username 保持一致。
// 前端的檢查只是提早給提示；後端才是唯一的權威，不一致時以後端的錯誤訊息為準。
// 注意：HTML 的 minlength/maxlength 以 UTF-16 單位計算，後端以字元計算，
// 含 emoji 等字元時兩者會不同，這是前端檢查不能取代後端的原因之一。
const USERNAME_MIN = 3;
const USERNAME_MAX = 32;
const USERNAME_PATTERN = "[a-z0-9_\\-]+"; // pattern 屬性會自動比對整個字串
const USERNAME_HINT = t("{min}–{max} characters: lowercase letters, digits, underscores and hyphens", { min: USERNAME_MIN, max: USERNAME_MAX });
const PASSWORD_MIN = 8;
const PASSWORD_MAX = 128;

// ==================== 登入 ====================

/**
 * 在 container 中顯示登入畫面。
 *
 * @param onLoggedIn 登入成功後呼叫，傳入目前的使用者（可能需要先修改密碼）
 * @param notice     顯示在表單上方的提示（例如「登入已過期」）
 */
export function renderLogin(
  container: HTMLElement,
  onLoggedIn: (user: User) => void,
  notice?: string,
): void {
  const usernameInput = h("input", {
    name: "username",
    type: "text",
    required: true,
    // 讓密碼管理器認得這是帳號欄位
    autocomplete: "username",
    // 手機鍵盤不要自動大寫第一個字母、不要拼字檢查
    autocapitalize: "none",
    spellcheck: "false",
  });
  const passwordInput = h("input", {
    name: "password",
    type: "password",
    required: true,
    autocomplete: "current-password",
    maxlength: PASSWORD_MAX,
  });
  const errorBox = errorContainer();
  const submitButton = h("button", { type: "submit" }, t("Log in"));

  // method="post"：萬一 JS 沒有載入、瀏覽器以原生方式送出表單，
  // 密碼也不會出現在網址裡（預設的 GET 會把它放進網址、瀏覽紀錄與伺服器 log）
  const form = h(
    "form",
    { method: "post", class: "auth-form" },
    h("h1", {}, t("Log in to ManorCloud")),
    notice ? h("p", { class: "notice", role: "status" }, notice) : null,
    labeled(t("Username"), usernameInput),
    labeled(t("Password"), passwordInput),
    errorBox,
    submitButton,
  );

  bindSubmit(
    form,
    submitButton,
    async () => {
      const data = new FormData(form);
      // 登入 API 的 401 是「帳號或密碼錯誤」，不是「登入已過期」
      const result = await api.post<User | { two_factor_required: true }>(
        "/api/login",
        {
          username: textField(data, "username"),
          password: textField(data, "password"),
        },
        { notifyUnauthorized: false },
      );
      if ("two_factor_required" in result) {
        // 密碼正確，帳號開啟了 2FA：這時才出現驗證碼的輸入框
        renderTwoFactorStep(container, onLoggedIn);
        return;
      }
      onLoggedIn(result);
    },
    (error) => {
      errorBox.textContent = messageOf(error);
      // 保留帳號，清空密碼，游標移到密碼欄方便重新輸入
      passwordInput.value = "";
      passwordInput.focus();
    },
  );

  container.replaceChildren(form);
  usernameInput.focus();
  void addSsoButton(form, errorBox);
}

/**
 * 登入的第二步：輸入驗證器 App 的驗證碼，或一組備用碼。
 *
 * - 驗證碼錯誤（400）：留在這裡再試
 * - 等待中的登入過期或猜錯太多次（401）：回到帳號密碼，並說明原因
 */
function renderTwoFactorStep(container: HTMLElement, onLoggedIn: (user: User) => void): void {
  const code = codeInput();
  const errorBox = errorContainer();
  const submitButton = h("button", { type: "submit" }, t("Verify"));
  const hint = h("p", {}, t("Enter the 6-digit code from your authenticator app."));
  let recovery = false;
  const toggle = h("button", { type: "button", class: "link" }, t("Use a recovery code instead"));
  toggle.addEventListener("click", () => {
    recovery = !recovery;
    hint.textContent = recovery
      ? t("Enter one of your recovery codes, like abcd-efgh. Each code works once.")
      : t("Enter the 6-digit code from your authenticator app.");
    toggle.textContent = recovery ? t("Use the authenticator app instead") : t("Use a recovery code instead");
    code.inputMode = recovery ? "text" : "numeric";
    code.value = "";
    code.focus();
  });
  const back = h("button", { type: "button", class: "link" }, t("Back"));
  back.addEventListener("click", () => renderLogin(container, onLoggedIn));

  const form = h(
    "form",
    { method: "post", class: "auth-form" },
    h("h1", {}, t("Two-factor authentication")),
    hint,
    labeled(t("Code"), code),
    errorBox,
    submitButton,
    h("div", { class: "auth-links" }, toggle, back),
  );

  bindSubmit(
    form,
    submitButton,
    async () => {
      const user = await api.post<User>(
        "/api/login/two-factor",
        { code: textField(new FormData(form), "code") },
        { notifyUnauthorized: false },
      );
      onLoggedIn(user);
    },
    (error) => {
      if (error instanceof ApiError && error.status === 401) {
        renderLogin(container, onLoggedIn, messageOf(error));
        return;
      }
      errorBox.textContent = messageOf(error);
      code.value = "";
      code.focus();
    },
  );

  code.name = "code";
  container.replaceChildren(form);
  code.focus();
}

/**
 * SSO 啟用時，在登入表單下方加上「Sign in with …」。
 * 讀不到設定時不顯示：密碼登入不受影響
 */
async function addSsoButton(form: HTMLElement, errorBox: HTMLElement): Promise<void> {
  let info;
  try {
    info = await ssoInfo();
  } catch {
    return;
  }
  if (!info.enabled || !form.isConnected) {
    return;
  }
  const button = h(
    "button",
    { type: "button", class: "secondary sso-button" },
    info.display_name === null ? t("Sign in with single sign-on") : t("Sign in with {name}", { name: info.display_name }),
  );
  button.addEventListener("click", () => {
    button.disabled = true;
    errorBox.textContent = "";
    startSso("login").catch((error: unknown) => {
      button.disabled = false;
      errorBox.textContent = messageOf(error);
    });
  });
  form.append(h("div", { class: "sso-divider" }, h("span", {}, "or")), button);
}

// ==================== 修改帳號名稱與密碼 ====================

interface ChangeCredentialsOptions {
  /**
   * 強制模式（首次登入、臨時密碼）：新密碼必填，沒有取消按鈕。
   * 自願模式（帳號設定）：可以只改名稱，有取消按鈕。
   */
  forced: boolean;
  /** 自願模式的「取消」 */
  onCancel?: () => void;
  /** 放在 Settings 頁面裡：標題是區塊名稱（Account），版面靠左而不是置中 */
  embedded?: boolean;
}

/**
 * 在 container 中顯示修改帳號名稱與密碼的畫面。
 *
 * @param onChanged 修改成功後呼叫，傳入更新後的使用者。
 *                  改了密碼時，伺服器已登出其他所有裝置，並透過 Set-Cookie 換了這個裝置的 session。
 */
export function renderChangeCredentials(
  container: HTMLElement,
  user: User,
  onChanged: (user: User) => void,
  { forced, onCancel, embedded = false }: ChangeCredentialsOptions,
): void {
  const usernameInput = h("input", {
    name: "new_username",
    type: "text",
    required: true,
    value: user.username,
    // 讓密碼管理器把新密碼與這個帳號名稱一起儲存
    autocomplete: "username",
    autocapitalize: "none",
    spellcheck: "false",
    minlength: USERNAME_MIN,
    maxlength: USERNAME_MAX,
    pattern: USERNAME_PATTERN,
    title: USERNAME_HINT,
  });
  const currentInput = h("input", {
    name: "current_password",
    type: "password",
    required: true,
    autocomplete: "current-password",
    maxlength: PASSWORD_MAX,
  });
  const newInput = h("input", {
    name: "new_password",
    type: "password",
    // 強制模式一定要設定新密碼；自願模式可以留空（只改名稱）
    required: forced,
    // new-password：瀏覽器會提議產生強密碼，並在成功後詢問是否儲存
    autocomplete: "new-password",
    minlength: PASSWORD_MIN,
    maxlength: PASSWORD_MAX,
    placeholder: forced ? undefined : t("Leave empty to keep your password"),
  });
  const confirmInput = h("input", {
    name: "confirm_password",
    type: "password",
    required: forced,
    autocomplete: "new-password",
    maxlength: PASSWORD_MAX,
  });
  const errorBox = errorContainer();
  const submitButton = h("button", { type: "submit" }, forced ? t("Finish setup") : t("Save changes"));

  const form = h(
    "form",
    { method: "post", class: embedded ? "auth-form embedded" : "auth-form" },
    embedded
      ? h("h2", { class: "section-title" }, t("Account"))
      : h("h1", {}, forced ? t("Set up your account") : t("Account settings")),
    forced
      ? h(
          "p",
          { class: "notice", role: "status" },
          t("You're using a temporary password. Choose your own password; you can also change your username. The temporary password stops working afterwards."),
        )
      : null,
    labeled(t("Username"), usernameInput, h("span", { class: "hint" }, USERNAME_HINT)),
    labeled(forced ? t("Current password (the temporary one)") : t("Current password"), currentInput),
    labeled(
      t("New password"),
      newInput,
      h("span", { class: "hint" }, t("{min}–{max} characters", { min: PASSWORD_MIN, max: PASSWORD_MAX })),
    ),
    labeled(t("Confirm new password"), confirmInput),
    errorBox,
    h(
      "div",
      { class: "actions" },
      submitButton,
      onCancel && !forced
        ? h("button", { type: "button", class: "secondary", onClick: onCancel }, t("Cancel"))
        : null,
    ),
  );

  bindSubmit(
    form,
    submitButton,
    async () => {
      const data = new FormData(form);
      const newUsername = textField(data, "new_username");
      const newPassword = textField(data, "new_password");

      if (newPassword !== textField(data, "confirm_password")) {
        throw new FormError(t("The new passwords don't match"), confirmInput);
      }

      // 只送出有改變的欄位
      const body: Record<string, string> = {
        current_password: textField(data, "current_password"),
      };
      if (newUsername !== user.username) {
        body.new_username = newUsername;
      }
      if (newPassword !== "") {
        body.new_password = newPassword;
      }
      if (!("new_username" in body) && !("new_password" in body)) {
        throw new FormError(t("Nothing to change"), usernameInput);
      }

      const updated = await api.post<User>("/api/me/credentials", body);
      onChanged(updated);
    },
    (error) => {
      errorBox.textContent = messageOf(error);
      // 密碼欄一律清空，避免明文停留在畫面上
      currentInput.value = "";
      newInput.value = "";
      confirmInput.value = "";

      // 依錯誤原因把游標放到最需要修改的欄位
      if (error instanceof FormError) {
        error.field.focus();
      } else if (error instanceof ApiError && error.code === "wrong_current_password") {
        currentInput.focus();
      } else if (error instanceof ApiError && error.status === 409) {
        usernameInput.focus();
      } else {
        currentInput.focus();
      }
    },
  );

  container.replaceChildren(form);
  (forced ? currentInput : usernameInput).focus();
}

// ==================== 共用 ====================

/** 前端自己檢查出的錯誤，附帶應該聚焦的欄位 */
class FormError extends Error {
  readonly field: HTMLElement;

  constructor(message: string, field: HTMLElement) {
    super(message);
    this.name = "FormError";
    this.field = field;
  }
}

/**
 * 綁定表單送出：阻止原生送出、防止重複送出，並保證按鈕一定恢復。
 *
 * @param run     送出時執行；拋出的任何錯誤都交給 onError
 * @param onError 顯示錯誤、重設欄位
 */
function bindSubmit(
  form: HTMLFormElement,
  button: HTMLButtonElement,
  run: () => Promise<void>,
  onError: (error: unknown) => void,
): void {
  const label = button.textContent ?? "";
  let busy = false;

  form.addEventListener("submit", (event) => {
    // 阻止瀏覽器以原生方式送出表單（那會重新載入整個頁面）
    event.preventDefault();
    // 按鈕停用後，按 Enter 也不會送出；這裡再加一道保險
    if (busy) {
      return;
    }

    busy = true;
    button.disabled = true;
    button.textContent = t("Working…");

    void (async () => {
      try {
        await run();
      } catch (error) {
        onError(error);
      } finally {
        // 不論成功、失敗或拋出未預期的例外，按鈕一定恢復
        busy = false;
        button.disabled = false;
        button.textContent = label;
      }
    })();
  });
}

/** <input> 包在 <label> 裡：點文字就能聚焦，螢幕閱讀器也知道它的名稱 */
function labeled(text: string, input: HTMLInputElement, hint?: HTMLElement): HTMLLabelElement {
  return h("label", {}, text, input, hint ?? null);
}

/** 錯誤訊息的容器。role="alert"：內容出現時，螢幕閱讀器會自動念出來 */
function errorContainer(): HTMLParagraphElement {
  return h("p", { class: "error", role: "alert" });
}

function messageOf(error: unknown): string {
  if (error instanceof ApiError || error instanceof FormError) {
    return error.message;
  }
  return t("Something went wrong");
}

/**
 * 從表單取出文字欄位。
 * FormData.get() 回傳 string | File | null：欄位不存在是 null，檔案欄位是 File。
 * strict 模式下要先確認是字串才能使用。
 */
function textField(data: FormData, name: string): string {
  const value = data.get(name);
  return typeof value === "string" ? value : "";
}
