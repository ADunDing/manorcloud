// Settings（每個人）與 Administration（管理員）兩個頁面：左邊是區塊清單，右邊是內容。
//
// 新的區塊（例如儲存位置的 Storage）：在 router.ts 的清單加上名稱，
// 再在這裡的清單與 switch 各加一項。

import { copyText } from "./clipboard.js";
import { languageSetting, setLanguage, t, tNodes, type LanguageSetting } from "./i18n.js";
import { renderAdmin } from "./admin-view.js";
import { renderStorage, type ServerSettings } from "./storage-view.js";
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
import {
  codeInput,
  renderTwoFactorSetup,
  showRecoveryCodes,
  type TwoFactorStatus,
} from "./two-factor-view.js";
import { showToast } from "./toast.js";
import type { User } from "./types.js";
import { renderWebDav } from "./webdav-view.js";

interface NavItem {
  readonly href: string;
  readonly label: string;
  readonly icon: IconName;
  readonly active: boolean;
}

// ==================== Settings ====================

const SETTINGS: readonly { section: SettingsSection; label: string; icon: IconName }[] = [
  { section: "account", label: t("Account"), icon: "user" },
  { section: "sign-in", label: t("Sign-in"), icon: "lock" },
  { section: "webdav", label: t("WebDAV"), icon: "link" },
  { section: "appearance", label: t("Appearance"), icon: "grid" },
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
    t("Settings"),
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
    case "webdav":
      renderWebDav(panel);
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
  panel.replaceChildren(h("p", { class: "muted" }, t("Loading…")));
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
      const name = status.display_name ?? "OIDC";
      panel.replaceChildren(
        h("h2", { class: "section-title" }, t("Account")),
        h(
          "div",
          { class: "settings-group" },
          h("p", {}, tNodes("Username: {name}", { name: h("strong", {}, user.username) })),
          h(
            "p",
            { class: "hint" },
            t("You sign in with {name}. This account has no password, so there is nothing to change here.", { name }),
          ),
        ),
      );
      return;
    }

    renderChangeCredentials(
      panel,
      user,
      (updated) => {
        showToast(t("Your account was updated"));
        onAccountChanged(updated);
      },
      { forced: false, embedded: true },
    );
  })();
}

/** Sign-in：兩步驟驗證與 SSO 的連結 */
function renderMySignIn(panel: HTMLElement): void {
  const twoFactor = h("section", { class: "settings-group" });
  const sso = h("section", { class: "settings-group" });
  panel.replaceChildren(h("h2", { class: "section-title" }, t("Sign-in")), twoFactor, sso);
  renderTwoFactorSection(twoFactor);
  renderSsoLink(sso);
}

/** 兩步驟驗證：狀態、開啟、換備用碼、關閉 */
function renderTwoFactorSection(section: HTMLElement): void {
  const title = h("h3", { class: "group-title" }, t("Two-factor authentication"));
  section.replaceChildren(title, h("p", { class: "muted" }, t("Loading…")));
  void (async () => {
    let status: TwoFactorStatus;
    try {
      status = await api.get<TwoFactorStatus>("/api/me/two-factor");
    } catch (error) {
      if (!isHandledGlobally(error)) {
        section.replaceChildren(title, h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
      return;
    }

    const body = h("div", {});
    const reload = (): void => renderTwoFactorSection(section);

    if (!status.available) {
      section.replaceChildren(
        title,
        h(
          "p",
          { class: "hint" },
          t("Your account signs in with single sign-on, so two-factor authentication is handled by your sign-in provider."),
        ),
      );
      return;
    }

    if (!status.enabled) {
      const turnOn = h("button", { type: "button", class: "primary" }, t("Turn on"));
      turnOn.addEventListener("click", () =>
        renderTwoFactorSetup(body, {
          onDone: () => {
            showToast(t("Two-factor authentication is on"));
            reload();
          },
          onCancel: reload,
        }),
      );
      body.replaceChildren(h("div", { class: "actions" }, turnOn));
      section.replaceChildren(
        title,
        h(
          "p",
          {},
          status.required
            ? t("Your administrator requires two-factor authentication.")
            : t("Protect your account with a code from your phone in addition to your password."),
        ),
        body,
      );
      return;
    }

    const newCodes = h("button", { type: "button", class: "secondary" }, t("New recovery codes"));
    newCodes.addEventListener("click", () => {
      const password = passwordInput();
      inlineForm(body, [[t("Current password"), password]], t("Create new codes"), false, reload, async () => {
        const result = await api.post<{ recovery_codes: string[] }>(
          "/api/me/two-factor/recovery-codes",
          { current_password: password.value },
        );
        showRecoveryCodes(body, result.recovery_codes, reload);
      });
    });

    const turnOff = h("button", { type: "button", class: "secondary" }, t("Turn off"));
    turnOff.addEventListener("click", () => {
      const password = passwordInput();
      const code = codeInput();
      inlineForm(
        body,
        [
          [t("Current password"), password],
          [t("Code from your app, or a recovery code"), code],
        ],
        t("Turn off"),
        true,
        reload,
        async () => {
          await api.post("/api/me/two-factor/disable", {
            current_password: password.value,
            code: code.value,
          });
          showToast(t("Two-factor authentication is off"));
          reload();
        },
      );
    });

    body.replaceChildren(
      h("div", { class: "actions" }, newCodes, status.required ? null : turnOff),
    );
    section.replaceChildren(
      title,
      h("p", {}, t("On. When you log in, you'll also enter a code from your authenticator app.")),
      h(
        "p",
        { class: "hint" },
        t("{count} of 10 recovery codes left.", { count: status.recovery_codes_left }),
        status.required
          ? ` ${t("Your administrator requires two-factor authentication, so it can't be turned off.")}`
          : "",
      ),
      body,
    );
  })();
}

/** 在 container 中顯示一個小表單（換備用碼、關閉 2FA 前確認身分） */
function inlineForm(
  container: HTMLElement,
  fields: readonly [string, HTMLInputElement][],
  confirmLabel: string,
  danger: boolean,
  onCancel: () => void,
  onSubmit: () => Promise<void>,
): void {
  const errorBox = h("p", { class: "error", role: "alert" });
  const confirm = h(
    "button",
    { type: "submit", class: danger ? "primary danger" : "primary" },
    confirmLabel,
  );
  const cancel = h("button", { type: "button", class: "secondary", onClick: onCancel }, t("Cancel"));
  const form = h(
    "form",
    { method: "post", class: "auth-form embedded" },
    fields.map(([label, input]) => h("label", {}, label, input)),
    errorBox,
    h("div", { class: "actions" }, confirm, cancel),
  );
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    confirm.disabled = true;
    errorBox.textContent = "";
    onSubmit().catch((error: unknown) => {
      confirm.disabled = false;
      if (!isHandledGlobally(error)) {
        errorBox.textContent = messageOf(error);
      }
    });
  });
  container.replaceChildren(form);
  fields[0]?.[1].focus();
}

function passwordInput(): HTMLInputElement {
  return h("input", { type: "password", required: true, autocomplete: "current-password" });
}

/** 自己的 SSO 連結：連結、取消連結，或說明還沒有設定 */
function renderSsoLink(panel: HTMLElement): void {
  panel.replaceChildren(h("p", { class: "muted" }, t("Loading…")));
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

    const name = status.display_name ?? "OIDC";
    const errorBox = h("p", { class: "error", role: "alert" });
    const content: Node[] = [];

    if (status.linked) {
      content.push(
        h(
          "p",
          {},
          status.has_password
            ? t("Your account is linked with {name}. You can sign in with {name} or with your password.", { name })
            : t("Your account is linked with {name}, and that's how you sign in.", { name }),
        ),
      );
      if (status.has_password) {
        const unlink = h("button", { type: "button", class: "secondary" }, t("Unlink {name}", { name }));
        unlink.addEventListener("click", () => {
          void (async () => {
            const confirmed = await confirmDialog({
              title: t("Unlink {name}?", { name }),
              message: t("You'll sign in with your password only. You can link your {name} account again at any time.", { name }),
              confirmLabel: t("Unlink"),
            });
            if (!confirmed) {
              return;
            }
            try {
              await api.delete("/api/me/sso");
              showToast(t("{name} unlinked", { name }));
              renderSsoLink(panel);
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
          h("p", { class: "hint" }, t("Your account has no password, so it can't be unlinked.")),
        );
      }
    } else if (status.enabled) {
      const link = h("button", { type: "button", class: "primary" }, t("Link {name} account", { name }));
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
        h("p", {}, t("Link your {name} account to sign in without typing your password.", { name })),
        h("div", { class: "actions" }, link),
      );
    } else {
      content.push(
        h(
          "p",
          { class: "hint" },
          t("Single sign-on isn't set up. An administrator can turn it on in Administration → Sign-in."),
        ),
      );
    }

    panel.replaceChildren(h("h3", { class: "group-title" }, t("Single sign-on")), ...content, errorBox);
  })();
}

function renderAppearance(panel: HTMLElement): void {
  const themes: [Theme, string, string][] = [
    ["system", t("Match my device"), t("Light or dark, following your computer's setting")],
    ["light", t("Light"), t("Always light")],
    ["dark", t("Dark"), t("Always dark")],
  ];
  const current = loadTheme();
  const themeGroup = group(
    t("Theme"),
    themes.map(([value, label, hint]) =>
      choice("theme", value === current, label, hint, () => {
        saveTheme(value);
        applyTheme(value);
      }),
    ),
  );

  const views: [ViewMode, string, string][] = [
    ["list", t("List"), t("Names, sizes and dates in rows")],
    ["grid", t("Grid"), t("Large icons in a grid")],
  ];
  const view = loadView();
  const viewGroup = group(
    t("File view"),
    views.map(([value, label, hint]) =>
      choice("view", value === view, label, hint, () => saveView(value)),
    ),
    t("You can also switch with the buttons at the top right of My files."),
  );

  // 語言：存在這台裝置。語言的名稱用各自的語言寫，不翻譯
  const languages: [LanguageSetting, string][] = [
    ["auto", t("Match my browser")],
    ["en", "English"],
    ["zh-TW", "繁體中文"],
  ];
  const currentLanguage = languageSetting();
  const languageGroup = group(
    t("Language"),
    languages.map(([value, label]) =>
      choice("language", value === currentLanguage, label, "", () => setLanguage(value)),
    ),
    t("Saved on this device. The page reloads when you change it."),
  );

  // 隱藏檔：存在伺服器上，每台裝置都一樣
  const hidden = h("input", { type: "checkbox", disabled: true });
  const hiddenError = h("p", { class: "error", role: "alert" });
  const hiddenGroup = h(
    "fieldset",
    { class: "settings-group" },
    h("legend", {}, t("Hidden files")),
    h(
      "div",
      { class: "choices" },
      choiceLabel(
        hidden,
        t("Show hidden files"),
        t("Files whose names start with a dot, such as .DS_Store from a Mac"),
      ),
    ),
    hiddenError,
  );
  void (async () => {
    try {
      const prefs = await api.get<{ show_hidden: boolean }>("/api/me/preferences");
      hidden.checked = prefs.show_hidden;
      hidden.disabled = false;
    } catch (error) {
      if (!isHandledGlobally(error)) {
        hiddenError.textContent = messageOf(error);
      }
    }
  })();
  hidden.addEventListener("change", () => {
    hiddenError.textContent = "";
    void api.put("/api/me/preferences", { show_hidden: hidden.checked }).catch((error: unknown) => {
      hidden.checked = !hidden.checked;
      if (!isHandledGlobally(error)) {
        hiddenError.textContent = messageOf(error);
      }
    });
  });

  panel.replaceChildren(
    h("h2", { class: "section-title" }, t("Appearance")),
    themeGroup,
    languageGroup,
    viewGroup,
    hiddenGroup,
  );
}

// ==================== Administration ====================

const ADMIN: readonly { section: AdminSection; label: string; icon: IconName }[] = [
  { section: "users", label: t("Users"), icon: "users" },
  { section: "storage", label: t("Storage"), icon: "disk" },
  { section: "two-factor", label: t("Two-factor authentication"), icon: "shield" },
  { section: "sso", label: t("Single sign-on"), icon: "key" },
  { section: "proxy", label: t("Reverse proxy"), icon: "link" },
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
    t("Administration"),
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
    case "storage":
      renderStorage(panel);
      break;
    case "two-factor":
      renderRequirement(panel);
      break;
    case "sso":
      renderSsoSettings(panel);
      break;
    case "proxy":
      renderTrustedProxies(panel);
      break;
  }
}

/** Administration → Single sign-on：SSO（OpenID Connect）的設定 */
function renderSsoSettings(page: HTMLElement): void {
  const panel = h("div", {});
  page.replaceChildren(h("h2", { class: "section-title" }, t("Single sign-on")), panel);
  panel.replaceChildren(h("p", { class: "muted" }, t("Loading…")));
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
    placeholder: "OIDC",
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
    placeholder: current.has_secret ? t("Saved. Leave empty to keep it") : "",
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
  const adminGroup = h("input", {
    type: "text",
    value: current.admin_group,
    placeholder: "authentik Admins",
    maxlength: 200,
    spellcheck: "false",
  });
  const testResult = h("p", { class: "hint", role: "status" });
  const errorBox = h("p", { class: "error", role: "alert" });
  const save = h("button", { type: "submit", class: "primary" }, t("Save"));

  // Redirect URI 跟著 ManorCloud 的網址即時更新（與伺服器的組法相同：去掉結尾的斜線）
  const updateRedirect = (): void => {
    const base = publicUrl.value.trim().replace(/\/+$/, "");
    redirect.value = base === "" ? "" : `${base}/api/auth/sso/callback`;
  };
  publicUrl.addEventListener("input", updateRedirect);
  updateRedirect();

  const copy = h("button", { type: "button", class: "secondary" }, t("Copy"));
  copy.addEventListener("click", () => {
    void copyInput(redirect).then((copied) => {
      if (copied) {
        showToast(t("Redirect URI copied"));
      }
    });
  });

  const test = h("button", { type: "button", class: "secondary" }, t("Test connection"));
  test.addEventListener("click", () => {
    test.disabled = true;
    testResult.textContent = t("Connecting…");
    testResult.classList.remove("error");
    void (async () => {
      try {
        await api.post("/api/admin/sso/test", { issuer_url: issuer.value.trim() });
        testResult.textContent = t("Connected: the provider's settings were read successfully.");
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
    h(
      "p",
      { class: "hint" },
      t("Let people sign in with an OpenID Connect provider such as Authentik. "),
      t("Create an OAuth2/OpenID provider there, then copy its values here."),
    ),
    h("label", { class: "inline" }, enabled, t("Turn on single sign-on")),
    field(t("Button label"), displayName, t("Shown on the login page as \"Sign in with …\"")),
    field(
      t("Issuer URL"),
      h("div", { class: "field-row" }, issuer, test),
      t("The provider's OpenID configuration issuer. Must start with https://"),
    ),
    testResult,
    field(t("Client ID"), clientId),
    field(t("Client secret"), clientSecret),
    field(
      t("ManorCloud address"),
      publicUrl,
      t("The address people use to open ManorCloud, for example https://cloud.example.com"),
    ),
    field(
      t("Redirect URI"),
      h("div", { class: "field-row" }, redirect, copy),
      t("Paste this into the provider's settings as the redirect URI"),
    ),
    h(
      "fieldset",
      { class: "settings-group" },
      h("legend", {}, t("People without a ManorCloud account")),
      h(
        "div",
        { class: "choices" },
        choiceLabel(autoCreate, t("Create an account"), t("They become a member on their first sign-in")),
        choiceLabel(refuse, t("Don't let them in"), t("An administrator creates their account first")),
      ),
    ),
    field(
      t("Administrator group"),
      adminGroup,
      t("Members of this group at the provider become administrators each time they sign in, and everyone else becomes a member. Leave empty to manage roles in ManorCloud."),
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
          admin_group: adminGroup.value.trim(),
        });
        showToast(t("Single sign-on settings saved"));
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

/** 是否強制每個用密碼登入的人開啟 2FA */
function renderRequirement(section: HTMLElement): void {
  const title = h("h2", { class: "section-title" }, t("Two-factor authentication"));
  section.replaceChildren(title, h("p", { class: "muted" }, t("Loading…")));
  void (async () => {
    let current: { require_two_factor: boolean };
    try {
      current = await api.get<{ require_two_factor: boolean }>("/api/admin/two-factor");
    } catch (error) {
      if (!isHandledGlobally(error)) {
        section.replaceChildren(title, h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
      return;
    }
    const required = h("input", { type: "checkbox", checked: current.require_two_factor });
    const errorBox = h("p", { class: "error", role: "alert" });
    const save = h("button", { type: "submit", class: "primary" }, t("Save"));
    const form = h(
      "form",
      { method: "post", class: "auth-form embedded settings-form" },
      h("label", { class: "inline" }, required, t("Require two-factor authentication")),
      h(
        "p",
        { class: "hint" },
        t("Everyone who logs in with a password must set it up before they can use ManorCloud. "),
        t("If you haven't set it up yourself, you'll be asked to right away. "),
        t("People who use single sign-on aren't affected."),
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
          await api.put("/api/admin/two-factor", { require_two_factor: required.checked });
          showToast(t("Saved"));
          // 打開強制而自己還沒設定：這個請求就會收到 two_factor_setup_required，
          // 全域處理會切換到設定畫面
          await api.get("/api/admin/two-factor");
          save.disabled = false;
        } catch (error) {
          if (!isHandledGlobally(error)) {
            errorBox.textContent = messageOf(error);
          }
          save.disabled = false;
        }
      })();
    });
    section.replaceChildren(title, form);
  })();
}

/**
 * 反向代理的位址：登入次數限制才能算在每個訪客頭上，而不是全部算在代理頭上。
 * 顯示這個請求的連線來自哪裡：放在代理後面時就是代理的位址，照著填就對了
 */
function renderTrustedProxies(section: HTMLElement): void {
  const title = h("h2", { class: "section-title" }, t("Reverse proxy"));
  section.replaceChildren(title, h("p", { class: "muted" }, t("Loading…")));
  void (async () => {
    let current: ServerSettings;
    try {
      current = await api.get<ServerSettings>("/api/admin/settings");
    } catch (error) {
      if (!isHandledGlobally(error)) {
        section.replaceChildren(title, h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
      return;
    }
    const input = h("input", {
      type: "text",
      value: current.trusted_proxies,
      placeholder: "192.168.1.2",
      maxlength: 1000,
      spellcheck: "false",
      "aria-label": t("Reverse proxy addresses"),
    });
    const errorBox = h("p", { class: "error", role: "alert" });
    const save = h("button", { type: "submit", class: "primary" }, t("Save"));
    const form = h(
      "form",
      { method: "post", class: "auth-form embedded settings-form" },
      h(
        "p",
        { class: "hint" },
        t("If people reach ManorCloud through a reverse proxy such as nginx or Caddy, enter its address. Failed logins are then counted for each visitor, instead of locking everyone out together. Separate several addresses or ranges with commas."),
      ),
      h("div", { class: "field-row" }, input, save),
      h(
        "p",
        { class: "hint" },
        tNodes("This page reached ManorCloud from {address}. If that's your reverse proxy, enter it here.", {
          address: h("strong", {}, current.connection_from),
        }),
      ),
      errorBox,
    );
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      save.disabled = true;
      errorBox.textContent = "";
      void (async () => {
        try {
          const saved = await api.put<{ trusted_proxies: string }>(
            "/api/admin/settings/trusted-proxies",
            { trusted_proxies: input.value },
          );
          input.value = saved.trusted_proxies;
          showToast(t("Saved"));
        } catch (error) {
          if (!isHandledGlobally(error)) {
            errorBox.textContent = messageOf(error);
          }
        } finally {
          save.disabled = false;
        }
      })();
    });
    section.replaceChildren(title, form);
  })();
}

/** 複製輸入框的文字。瀏覽器不允許時選取文字，讓使用者自己複製 */
async function copyInput(input: HTMLInputElement): Promise<boolean> {
  if (await copyText(input.value, input)) {
    return true;
  }
  input.focus();
  input.select();
  return false;
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
  return error instanceof ApiError ? error.message : t("Something went wrong");
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
