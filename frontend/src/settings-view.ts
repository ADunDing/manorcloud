// Settings（每個人）與 Administration（管理員）兩個頁面：左邊是區塊清單，右邊是內容。
//
// 新的區塊（例如儲存位置的 Storage）：在 router.ts 的清單加上名稱，
// 再在這裡的清單與 switch 各加一項。

import { renderAdmin } from "./admin-view.js";
import { api, ApiError, isHandledGlobally } from "./api.js";
import { renderChangeCredentials } from "./auth-view.js";
import { confirmDialog } from "./dialogs.js";
import { h } from "./dom.js";
import { loadView, saveView, type ViewMode } from "./file-list.js";
import { icon, type IconName } from "./icons.js";
import {
  adminHref,
  settingsHref,
  type AdminSection,
  type SettingsSection,
} from "./router.js";
import { startSso, type AdminSso, type MySso } from "./sso.js";
import { applyTheme, loadTheme, saveTheme, type Theme } from "./theme.js";
import { showToast } from "./toast.js";
import type { User } from "./types.js";

interface NavItem {
  readonly href: string;
  readonly label: string;
  readonly icon: IconName;
  readonly active: boolean;
}

// ==================== Settings ====================

const SETTINGS: readonly { section: SettingsSection; label: string; icon: IconName }[] = [
  { section: "account", label: "Account", icon: "user" },
  { section: "sign-in", label: "Sign-in", icon: "lock" },
  { section: "appearance", label: "Appearance", icon: "grid" },
];

/**
 * @param onAccountChanged 帳號名稱或密碼修改成功後呼叫，傳入更新後的使用者
 */
export function renderSettings(
  container: HTMLElement,
  user: User,
  section: SettingsSection,
  onAccountChanged: (user: User) => void,
): void {
  const panel = layout(
    container,
    "Settings",
    SETTINGS.map((item) => ({
      href: settingsHref(item.section),
      label: item.label,
      icon: item.icon,
      active: item.section === section,
    })),
  );

  switch (section) {
    case "account":
      renderAccount(panel, user, onAccountChanged);
      break;
    case "sign-in":
      renderMySignIn(panel);
      break;
    case "appearance":
      renderAppearance(panel);
      break;
  }
}

/**
 * 帳號名稱與密碼。只用 SSO 的帳號沒有密碼：顯示名稱與說明，不顯示密碼表單
 * （修改都需要目前的密碼）
 */
function renderAccount(
  panel: HTMLElement,
  user: User,
  onAccountChanged: (user: User) => void,
): void {
  panel.replaceChildren(h("p", { class: "muted" }, "Loading…"));
  void (async () => {
    let status: MySso | null = null;
    try {
      status = await api.get<MySso>("/api/me/sso");
    } catch (error) {
      if (isHandledGlobally(error)) {
        return;
      }
      // 讀不到時當作有密碼：表單本身仍會要求目前的密碼
    }

    if (status !== null && !status.has_password) {
      const name = status.display_name ?? "single sign-on";
      panel.replaceChildren(
        h("h2", { class: "section-title" }, "Account"),
        h(
          "div",
          { class: "settings-group" },
          h("p", {}, "Username: ", h("strong", {}, user.username)),
          h(
            "p",
            { class: "hint" },
            `You sign in with ${name}. This account has no password, so there is nothing to change here.`,
          ),
        ),
      );
      return;
    }

    renderChangeCredentials(
      panel,
      user,
      (updated) => {
        showToast("Your account was updated");
        onAccountChanged(updated);
      },
      { forced: false, embedded: true },
    );
  })();
}

/** 自己的 SSO 連結：連結、取消連結，或說明還沒有設定 */
function renderMySignIn(panel: HTMLElement): void {
  panel.replaceChildren(h("p", { class: "muted" }, "Loading…"));
  void (async () => {
    let status: MySso;
    try {
      status = await api.get<MySso>("/api/me/sso");
    } catch (error) {
      if (!isHandledGlobally(error)) {
        panel.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
      return;
    }

    const name = status.display_name ?? "single sign-on";
    const errorBox = h("p", { class: "error", role: "alert" });
    const content: Node[] = [];

    if (status.linked) {
      content.push(
        h(
          "p",
          {},
          status.has_password
            ? `Your account is linked with ${name}. You can sign in with ${name} or with your password.`
            : `Your account is linked with ${name}, and that's how you sign in.`,
        ),
      );
      if (status.has_password) {
        const unlink = h("button", { type: "button", class: "secondary" }, `Unlink ${name}`);
        unlink.addEventListener("click", () => {
          void (async () => {
            const confirmed = await confirmDialog({
              title: `Unlink ${name}?`,
              message: `You'll sign in with your password only. You can link your ${name} account again at any time.`,
              confirmLabel: "Unlink",
            });
            if (!confirmed) {
              return;
            }
            try {
              await api.delete("/api/me/sso");
              showToast(`${name} unlinked`);
              renderMySignIn(panel);
            } catch (error) {
              if (!isHandledGlobally(error)) {
                errorBox.textContent = messageOf(error);
              }
            }
          })();
        });
        content.push(h("div", { class: "actions" }, unlink));
      } else {
        content.push(
          h("p", { class: "hint" }, "Your account has no password, so it can't be unlinked."),
        );
      }
    } else if (status.enabled) {
      const link = h("button", { type: "button", class: "primary" }, `Link ${name} account`);
      link.addEventListener("click", () => {
        link.disabled = true;
        errorBox.textContent = "";
        startSso("link").catch((error: unknown) => {
          link.disabled = false;
          if (!isHandledGlobally(error)) {
            errorBox.textContent = messageOf(error);
          }
        });
      });
      content.push(
        h("p", {}, `Link your ${name} account to sign in without typing your password.`),
        h("div", { class: "actions" }, link),
      );
    } else {
      content.push(
        h(
          "p",
          { class: "hint" },
          "Single sign-on isn't set up. An administrator can turn it on in Administration → Sign-in.",
        ),
      );
    }

    panel.replaceChildren(
      h("h2", { class: "section-title" }, "Sign-in"),
      h("div", { class: "settings-group" }, ...content, errorBox),
    );
  })();
}

function renderAppearance(panel: HTMLElement): void {
  const themes: [Theme, string, string][] = [
    ["system", "Match my device", "Light or dark, following your computer's setting"],
    ["light", "Light", "Always light"],
    ["dark", "Dark", "Always dark"],
  ];
  const current = loadTheme();
  const themeGroup = group(
    "Theme",
    themes.map(([value, label, hint]) =>
      choice("theme", value === current, label, hint, () => {
        saveTheme(value);
        applyTheme(value);
      }),
    ),
  );

  const views: [ViewMode, string, string][] = [
    ["list", "List", "Names, sizes and dates in rows"],
    ["grid", "Grid", "Large icons in a grid"],
  ];
  const view = loadView();
  const viewGroup = group(
    "File view",
    views.map(([value, label, hint]) =>
      choice("view", value === view, label, hint, () => saveView(value)),
    ),
    "You can also switch with the buttons at the top right of My files.",
  );

  panel.replaceChildren(h("h2", { class: "section-title" }, "Appearance"), themeGroup, viewGroup);
}

// ==================== Administration ====================

const ADMIN: readonly { section: AdminSection; label: string; icon: IconName }[] = [
  { section: "users", label: "Users", icon: "users" },
  { section: "sign-in", label: "Sign-in", icon: "lock" },
];

/**
 * @param onSelfChanged 修改了自己的角色之後呼叫（例如降級為成員，已經不能再看這一頁）
 */
export function renderAdministration(
  container: HTMLElement,
  me: User,
  section: AdminSection,
  onSelfChanged: () => void,
): void {
  const panel = layout(
    container,
    "Administration",
    ADMIN.map((item) => ({
      href: adminHref(item.section),
      label: item.label,
      icon: item.icon,
      active: item.section === section,
    })),
  );

  switch (section) {
    case "users":
      renderAdmin(panel, me, onSelfChanged);
      break;
    case "sign-in":
      renderSsoSettings(panel);
      break;
  }
}

/** Administration → Sign-in：SSO（OpenID Connect）的設定 */
function renderSsoSettings(panel: HTMLElement): void {
  panel.replaceChildren(h("p", { class: "muted" }, "Loading…"));
  void (async () => {
    let current: AdminSso;
    try {
      current = await api.get<AdminSso>("/api/admin/sso");
    } catch (error) {
      if (!isHandledGlobally(error)) {
        panel.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
      return;
    }
    showSsoForm(panel, current);
  })();
}

function showSsoForm(panel: HTMLElement, current: AdminSso): void {
  const enabled = h("input", { type: "checkbox", checked: current.enabled });
  const displayName = h("input", {
    type: "text",
    value: current.display_name,
    placeholder: "Authentik",
    maxlength: 40,
  });
  const issuer = h("input", {
    type: "url",
    value: current.issuer_url,
    placeholder: "https://auth.example.com/application/o/manorcloud/",
    spellcheck: "false",
  });
  const clientId = h("input", { type: "text", value: current.client_id, spellcheck: "false" });
  const clientSecret = h("input", {
    type: "password",
    autocomplete: "off",
    placeholder: current.has_secret ? "Saved. Leave empty to keep it" : "",
  });
  // 還沒設定時，預先填入目前開啟 ManorCloud 的網址
  const publicUrl = h("input", {
    type: "url",
    value: current.public_url === "" ? window.location.origin : current.public_url,
    spellcheck: "false",
  });
  const redirect = h("input", { type: "text", readonly: true, class: "copy-field" });
  const autoCreate = h("input", { type: "radio", name: "new-users", checked: current.auto_create });
  const refuse = h("input", { type: "radio", name: "new-users", checked: !current.auto_create });
  const testResult = h("p", { class: "hint", role: "status" });
  const errorBox = h("p", { class: "error", role: "alert" });
  const save = h("button", { type: "submit", class: "primary" }, "Save");

  // Redirect URI 跟著 ManorCloud 的網址即時更新（與伺服器的組法相同：去掉結尾的斜線）
  const updateRedirect = (): void => {
    const base = publicUrl.value.trim().replace(/\/+$/, "");
    redirect.value = base === "" ? "" : `${base}/api/auth/sso/callback`;
  };
  publicUrl.addEventListener("input", updateRedirect);
  updateRedirect();

  const copy = h("button", { type: "button", class: "secondary" }, "Copy");
  copy.addEventListener("click", () => {
    void copyText(redirect).then((copied) => {
      if (copied) {
        showToast("Redirect URI copied");
      }
    });
  });

  const test = h("button", { type: "button", class: "secondary" }, "Test connection");
  test.addEventListener("click", () => {
    test.disabled = true;
    testResult.textContent = "Connecting…";
    testResult.classList.remove("error");
    void (async () => {
      try {
        await api.post("/api/admin/sso/test", { issuer_url: issuer.value.trim() });
        testResult.textContent = "Connected: the provider's settings were read successfully.";
      } catch (error) {
        if (isHandledGlobally(error)) {
          return;
        }
        testResult.textContent = messageOf(error);
        testResult.classList.add("error");
      } finally {
        test.disabled = false;
      }
    })();
  });

  const form = h(
    "form",
    { method: "post", class: "auth-form embedded settings-form" },
    h("h2", { class: "section-title" }, "Sign-in"),
    h(
      "p",
      { class: "hint" },
      "Let people sign in with an OpenID Connect provider such as Authentik. ",
      "Create an OAuth2/OpenID provider there, then copy its values here.",
    ),
    h("label", { class: "inline" }, enabled, "Turn on single sign-on"),
    field("Button label", displayName, "Shown on the login page as \"Sign in with …\""),
    field(
      "Issuer URL",
      h("div", { class: "field-row" }, issuer, test),
      "The provider's OpenID configuration issuer. Must start with https://",
    ),
    testResult,
    field("Client ID", clientId),
    field("Client secret", clientSecret),
    field(
      "ManorCloud address",
      publicUrl,
      "The address people use to open ManorCloud, for example https://cloud.example.com",
    ),
    field(
      "Redirect URI",
      h("div", { class: "field-row" }, redirect, copy),
      "Paste this into the provider's settings as the redirect URI",
    ),
    h(
      "fieldset",
      { class: "settings-group" },
      h("legend", {}, "People without a ManorCloud account"),
      h(
        "div",
        { class: "choices" },
        choiceLabel(autoCreate, "Create an account", "They become a member on their first sign-in"),
        choiceLabel(refuse, "Don't let them in", "An administrator creates their account first"),
      ),
    ),
    errorBox,
    h("div", { class: "actions" }, save),
  );

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    save.disabled = true;
    errorBox.textContent = "";
    void (async () => {
      try {
        const secret = clientSecret.value.trim();
        const saved = await api.put<AdminSso>("/api/admin/sso", {
          enabled: enabled.checked,
          display_name: displayName.value.trim(),
          issuer_url: issuer.value.trim(),
          client_id: clientId.value.trim(),
          // 空的：沿用已儲存的 secret
          ...(secret === "" ? {} : { client_secret: secret }),
          public_url: publicUrl.value.trim(),
          auto_create: autoCreate.checked,
        });
        showToast("Single sign-on settings saved");
        showSsoForm(panel, saved);
      } catch (error) {
        if (!isHandledGlobally(error)) {
          errorBox.textContent = messageOf(error);
        }
        save.disabled = false;
      }
    })();
  });

  panel.replaceChildren(form);
}

/** 複製文字。http 的網址中瀏覽器不提供剪貼簿 API：改成選取文字，讓使用者自己複製 */
async function copyText(input: HTMLInputElement): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(input.value);
    return true;
  } catch {
    input.focus();
    input.select();
    return false;
  }
}

function field(label: string, control: HTMLElement, hint?: string): HTMLElement {
  return h(
    "label",
    {},
    label,
    control,
    hint === undefined ? null : h("span", { class: "hint" }, hint),
  );
}

function choiceLabel(input: HTMLInputElement, label: string, hint: string): HTMLElement {
  return h(
    "label",
    { class: "choice" },
    input,
    h("span", { class: "choice-text" }, h("strong", {}, label), h("span", { class: "hint" }, hint)),
  );
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "Something went wrong";
}

// ==================== 共用的版面 ====================

/** 標題、左邊的區塊清單與右邊的內容區；回傳內容區，由各區塊填入 */
function layout(container: HTMLElement, title: string, items: readonly NavItem[]): HTMLElement {
  const panel = h("section", { class: "settings-panel" });
  container.replaceChildren(
    h("h1", { class: "page-title" }, title),
    h(
      "div",
      { class: "settings-page" },
      h(
        "nav",
        { class: "settings-nav", "aria-label": `${title} sections` },
        h(
          "ul",
          {},
          items.map((item) =>
            h(
              "li",
              {},
              h(
                "a",
                {
                  href: item.href,
                  class: "settings-link",
                  "aria-current": item.active ? "page" : null,
                },
                icon(item.icon, 18),
                item.label,
              ),
            ),
          ),
        ),
      ),
      panel,
    ),
  );
  return panel;
}

function group(legend: string, choices: HTMLElement[], note?: string): HTMLElement {
  return h(
    "fieldset",
    { class: "settings-group" },
    h("legend", {}, legend),
    h("div", { class: "choices" }, choices),
    note === undefined ? null : h("p", { class: "hint" }, note),
  );
}

/** 一個選項。選了立刻生效，不需要按儲存 */
function choice(
  name: string,
  checked: boolean,
  label: string,
  hint: string,
  onSelect: () => void,
): HTMLElement {
  const input = h("input", { type: "radio", name, checked });
  input.addEventListener("change", () => {
    if (input.checked) {
      onSelect();
    }
  });
  return h(
    "label",
    { class: "choice" },
    input,
    h("span", { class: "choice-text" }, h("strong", {}, label), h("span", { class: "hint" }, hint)),
  );
}
