// Settings → WebDAV：連線用的網址與帳號，以及應用程式密碼（每台裝置或 App 一組）。
// 密碼只在建立時顯示一次：伺服器只存雜湊。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { copyText } from "./clipboard.js";
import { confirmDialog } from "./dialogs.js";
import { h } from "./dom.js";
import { formatTime } from "./format.js";
import { t } from "./i18n.js";
import { showToast } from "./toast.js";

/** GET /api/me/app-passwords 的一項 */
interface AppPassword {
  readonly id: number;
  readonly name: string;
  readonly created_at: number;
  readonly last_used_at: number | null;
}

interface AppPasswords {
  readonly webdav_enabled: boolean;
  readonly username: string;
  readonly items: readonly AppPassword[];
}

/** 名稱最多幾個字元，與伺服器相同 */
const NAME_MAX = 100;

export function renderWebDav(panel: HTMLElement): void {
  const title = h("h2", { class: "section-title" }, t("WebDAV"));
  panel.replaceChildren(title, h("p", { class: "muted" }, t("Loading…")));
  void (async () => {
    let data: AppPasswords;
    try {
      data = await api.get<AppPasswords>("/api/me/app-passwords");
    } catch (error) {
      if (!isHandledGlobally(error)) {
        panel.replaceChildren(title, h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
      return;
    }
    const reload = (): void => renderWebDav(panel);

    if (!data.webdav_enabled) {
      panel.replaceChildren(
        title,
        h(
          "p",
          { class: "hint" },
          t("WebDAV is turned off. Ask your administrator to turn it on in Administration → Storage."),
        ),
        ...(data.items.length > 0 ? [passwordList(data.items, reload)] : []),
      );
      return;
    }

    panel.replaceChildren(
      title,
      h(
        "section",
        { class: "settings-group" },
        h("h3", { class: "group-title" }, t("Connect")),
        h(
          "p",
          {},
          t("Open your files as a network drive in Finder (Go → Connect to Server) or File Explorer (Map network drive), or sync apps such as Obsidian, Joplin, KeePassXC and rclone with them."),
        ),
        field(t("Address"), copyRow(`${window.location.origin}/dav/`, t("Address copied"))),
        field(t("Username"), copyRow(data.username, t("Username copied"))),
        h(
          "p",
          { class: "hint" },
          t("For the password, create an app password below. Your login password doesn't work here."),
        ),
      ),
      passwordsSection(data.items, reload),
    );
  })();
}

/** 應用程式密碼：建立與清單 */
function passwordsSection(items: readonly AppPassword[], reload: () => void): HTMLElement {
  const name = h("input", {
    type: "text",
    required: true,
    maxlength: NAME_MAX,
    placeholder: "MacBook Finder",
    "aria-label": t("Name"),
  });
  const create = h("button", { type: "submit", class: "primary" }, t("Create"));
  const errorBox = h("p", { class: "error", role: "alert" });
  const slot = h("div", {});
  const form = h(
    "form",
    { method: "post", class: "auth-form embedded settings-form" },
    h("label", {}, t("Name"), h("div", { class: "field-row" }, name, create)),
    errorBox,
  );
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    create.disabled = true;
    errorBox.textContent = "";
    void (async () => {
      try {
        const created = await api.post<AppPassword & { password: string }>(
          "/api/me/app-passwords",
          { name: name.value.trim() },
        );
        showNewPassword(slot, created.name, created.password, reload);
      } catch (error) {
        create.disabled = false;
        if (!isHandledGlobally(error)) {
          errorBox.textContent = messageOf(error);
        }
      }
    })();
  });
  slot.replaceChildren(form);

  return h(
    "section",
    { class: "settings-group" },
    h("h3", { class: "group-title" }, t("App passwords")),
    h(
      "p",
      { class: "hint" },
      t("Create one for each device or app. If a device is lost, revoke just that one."),
    ),
    slot,
    items.length > 0 ? passwordList(items, reload) : null,
  );
}

/** 剛建立的密碼：只顯示這一次 */
function showNewPassword(
  slot: HTMLElement,
  name: string,
  password: string,
  reload: () => void,
): void {
  const done = h("button", { type: "button", class: "primary" }, t("Done"));
  done.addEventListener("click", reload);
  slot.replaceChildren(
    h(
      "div",
      { class: "auth-form embedded settings-form", role: "status" },
      h("p", {}, t("The app password for {name}. Copy it now: it won't be shown again.", { name })),
      copyRow(password, t("App password copied")),
      h("div", { class: "actions" }, done),
    ),
  );
  done.focus();
}

function passwordList(items: readonly AppPassword[], reload: () => void): HTMLElement {
  return h(
    "div",
    { class: "users-wrap" },
    h(
      "table",
      { class: "users" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", { scope: "col" }, t("Name")),
          h("th", { scope: "col" }, t("Created")),
          h("th", { scope: "col" }, t("Last used")),
          h("th", { scope: "col" }, h("span", { class: "visually-hidden" }, t("Actions"))),
        ),
      ),
      h(
        "tbody",
        {},
        items.map((item) => {
          const revoke = h("button", { type: "button", class: "secondary" }, t("Revoke"));
          revoke.addEventListener("click", () => void revokePassword(item, reload));
          return h(
            "tr",
            {},
            h("td", {}, item.name),
            h("td", { class: "nowrap" }, formatTime(item.created_at)),
            h(
              "td",
              { class: "nowrap" },
              item.last_used_at === null ? t("Not used yet") : formatTime(item.last_used_at),
            ),
            h("td", { class: "actions-cell" }, revoke),
          );
        }),
      ),
    ),
  );
}

async function revokePassword(item: AppPassword, reload: () => void): Promise<void> {
  const confirmed = await confirmDialog({
    title: t("Revoke app password?"),
    message: t("Devices and apps using {name} will no longer be able to connect.", { name: item.name }),
    confirmLabel: t("Revoke"),
    danger: true,
  });
  if (!confirmed) {
    return;
  }
  try {
    await api.delete(`/api/me/app-passwords/${item.id}`);
    showToast(t("App password revoked"));
  } catch (error) {
    if (!isHandledGlobally(error)) {
      showToast(messageOf(error));
    }
  }
  reload();
}

/** 唯讀的文字與 Copy 按鈕 */
function copyRow(value: string, copiedMessage: string): HTMLElement {
  const input = h("input", { type: "text", readonly: true, class: "copy-field", value });
  const copy = h("button", { type: "button", class: "secondary" }, t("Copy"));
  copy.addEventListener("click", () => {
    void copyText(value, input).then((copied) => {
      if (copied) {
        showToast(copiedMessage);
      } else {
        input.focus();
        input.select();
      }
    });
  });
  return h("div", { class: "field-row" }, input, copy);
}

function field(label: string, control: HTMLElement): HTMLElement {
  return h("label", {}, label, control);
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : t("Something went wrong");
}
