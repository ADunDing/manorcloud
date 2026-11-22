// 分享連結：
// - shareDialog：建立連結（可以設密碼與期限），建立後顯示連結讓使用者複製
// - renderShares：「Shared links」頁面，列出自己的分享，可以複製或停止分享
// - renderPublicShare：拿到連結的人看到的頁面（不需要登入），在 /s/{代碼}
//
// 伺服器的說明見 src/web/shares.rs。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { copyText } from "./clipboard.js";
import { h } from "./dom.js";
import { fileIconName, folderIconName, iconUrl } from "./file-types.js";
import { formatSize, formatTime } from "./format.js";
import { t } from "./i18n.js";
import { icon } from "./icons.js";
import { apiUrl, type Space } from "./space.js";
import { showToast } from "./toast.js";
import type { StorageOverview } from "./types.js";

/** GET /api/shares 的一筆。對應 Rust 端 web/shares.rs 的 ShareResponse */
interface Share {
  readonly id: number;
  readonly token: string;
  readonly space: string;
  readonly path: string;
  readonly name: string;
  readonly kind: "file" | "folder";
  readonly has_password: boolean;
  /** 只能下載一次 */
  readonly one_time: boolean;
  readonly expires_at: number | null;
  readonly expired: boolean;
  readonly downloads: number;
  readonly created_at: number;
}

const DAY = 24 * 60 * 60;

/** 連結的完整網址 */
function shareUrl(token: string): string {
  return `${window.location.origin}/s/${token}`;
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : t("Something went wrong");
}

/** 複製連結。near：觸發的按鈕（對話框中的按鈕要讓暫時的文字框放在同一個對話框） */
async function copyLink(text: string, near: Element, input?: HTMLInputElement): Promise<void> {
  if (await copyText(text, near)) {
    showToast(t("Link copied"));
    return;
  }
  input?.select();
  showToast(t("Selected. Press Ctrl+C (⌘C on a Mac) to copy"));
}

// ==================== 建立連結 ====================

/** 分享一個檔案或資料夾 */
export function shareDialog(space: Space, path: string, name: string, kind: "file" | "folder"): void {
  const usePassword = h("input", { type: "checkbox" });
  // 只能下載一次：只有檔案可以（資料夾裡有很多檔案，下載一個就失效會很奇怪）
  const oneTime = h("input", { type: "checkbox" });
  const password = h("input", {
    type: "text",
    class: "dialog-input",
    autocomplete: "off",
    spellcheck: "false",
    "aria-label": t("Password"),
    disabled: true,
  });
  usePassword.addEventListener("change", () => {
    password.disabled = !usePassword.checked;
    if (usePassword.checked) {
      password.focus();
    }
  });
  // 期限：數字加單位（分鐘、小時、天），或永不過期
  const amount = h("input", {
    type: "number",
    class: "dialog-input expiry-amount",
    min: "1",
    max: "9999",
    step: "1",
    value: "7",
    "aria-label": t("How long the link works"),
  });
  const unit = h(
    "select",
    { class: "dialog-input", "aria-label": t("Unit") },
    h("option", { value: "60" }, t("minutes")),
    h("option", { value: "3600" }, t("hours")),
    h("option", { value: String(DAY), selected: true }, t("days")),
    h("option", { value: "0" }, t("Never expires")),
  );
  unit.addEventListener("change", () => {
    amount.disabled = unit.value === "0";
  });
  const expiry = h("div", { class: "expiry-row" }, amount, unit);
  const error = h("p", { class: "error", role: "alert" });
  const cancel = h("button", { type: "button", class: "secondary" }, t("Cancel"));
  const create = h("button", { type: "submit", class: "primary" }, t("Create link"));
  const body = h(
    "div",
    { class: "share-options" },
    h("label", { class: "dialog-field" }, t("Link expires"), expiry),
    kind === "file"
      ? h(
          "label",
          { class: "inline", title: t("The link stops working as soon as the download starts. If the download fails, create a new link.") },
          oneTime,
          t("Allow only one download"),
        )
      : null,
    h("label", { class: "inline" }, usePassword, t("Require a password")),
    password,
  );
  const actions = h("div", { class: "dialog-actions" }, cancel, create);
  const form = h(
    "form",
    { method: "dialog", class: "dialog-form" },
    h("h2", { class: "dialog-title" }, t("Share \"{name}\"", { name })),
    h(
      "p",
      { class: "dialog-message" },
      t("Anyone with the link can view and download it, without an account."),
    ),
    body,
    error,
    actions,
  );
  const dialog = h("dialog", { class: "dialog" }, form);
  cancel.addEventListener("click", () => dialog.close());
  dialog.addEventListener("close", () => dialog.remove());

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const seconds = Number(unit.value);
    const count = Number(amount.value);
    if (seconds > 0 && !(Number.isInteger(count) && count >= 1 && count <= 9999)) {
      error.textContent = t("Enter how long the link works: a whole number from 1 to 9999");
      return;
    }
    if (usePassword.checked && password.value === "") {
      error.textContent = t("Enter a password, or turn off the password");
      return;
    }
    create.disabled = true;
    error.textContent = "";
    void (async () => {
      let share: Share;
      try {
        share = await api.post<Share>(apiUrl("/api/shares", space), {
          path,
          password: usePassword.checked ? password.value : null,
          one_time: kind === "file" && oneTime.checked,
          expires_at: seconds > 0 ? Math.floor(Date.now() / 1000) + count * seconds : null,
        });
      } catch (e) {
        create.disabled = false;
        if (!isHandledGlobally(e)) {
          error.textContent = messageOf(e);
        }
        return;
      }
      // 換成顯示連結
      const link = h("input", {
        type: "text",
        class: "dialog-input share-link",
        readonly: true,
        value: shareUrl(share.token),
        "aria-label": t("Link"),
      });
      const copy = h(
        "button",
        {
          type: "button",
          class: "primary",
          onClick: (event: Event) => void copyLink(link.value, event.currentTarget as Element, link),
        },
        t("Copy link"),
      );
      const done = h("button", { type: "button", class: "secondary", onClick: () => dialog.close() }, t("Done"));
      body.replaceChildren(
        link,
        h(
          "p",
          { class: "hint" },
          share.expires_at === null
            ? t("The link doesn't expire. Stop sharing it any time in Shared links.")
            : t("The link works until {date}. Stop sharing it any time in Shared links.", {
                date: formatTime(share.expires_at),
              }),
        ),
      );
      actions.replaceChildren(done, copy);
      link.addEventListener("focus", () => link.select());
      copy.focus();
    })();
  });

  document.body.append(dialog);
  dialog.showModal();
  amount.focus();
}

// ==================== Shared links 頁面 ====================

/** 自己的分享 */
export function renderShares(container: HTMLElement, admin: boolean): void {
  // space= → 顯示的名稱。管理員可能分享了其他硬碟上的檔案
  const labels = new Map<string, string>([["me", t("My files")]]);
  const spaceLabel = (space: string): string => labels.get(space) ?? t("Disk");
  const list = h("div", {}, h("p", { class: "muted" }, t("Loading…")));
  const errorBox = h("p", { class: "error", role: "alert" });
  container.replaceChildren(
    h("div", { class: "folder-header" }, h("h2", { class: "section-title" }, t("Shared links"))),
    h(
      "p",
      { class: "hint" },
      t("Anyone with one of these links can view and download what it points to. A link stops working when it expires, when you stop sharing it, or when the item is renamed, moved or deleted."),
    ),
    errorBox,
    list,
  );

  const load = async (): Promise<void> => {
    let shares: Share[];
    try {
      if (admin && labels.size === 1) {
        const storage = await api.get<StorageOverview>("/api/admin/storage");
        for (const volume of storage.volumes) {
          labels.set(volume.id === null ? "default" : String(volume.id), volume.name);
        }
      }
      shares = await api.get<Share[]>("/api/shares");
    } catch (error) {
      if (!isHandledGlobally(error)) {
        list.replaceChildren();
        errorBox.textContent = messageOf(error);
      }
      return;
    }
    if (shares.length === 0) {
      list.replaceChildren(
        h("p", { class: "empty-state" }, t("You haven't shared anything yet. Right-click a file or folder and choose Share.")),
      );
      return;
    }
    list.replaceChildren(
      h(
        "div",
        { class: "users-wrap" },
        h(
          "table",
          { class: "users shares" },
          h(
            "thead",
            {},
            h(
              "tr",
              {},
              h("th", { scope: "col" }, t("Name")),
              h("th", { scope: "col" }, t("Location")),
              h("th", { scope: "col" }, t("Expires")),
              h("th", { scope: "col", class: "num" }, t("Downloads")),
              h("th", { scope: "col" }, h("span", { class: "visually-hidden" }, t("Actions"))),
            ),
          ),
          h("tbody", {}, shares.map((share) => row(share))),
        ),
      ),
    );
  };

  const row = (share: Share): HTMLElement => {
    const parent = share.path.split("/").slice(0, -1);
    const stop = h(
      "button",
      { type: "button", class: "link danger-link" },
      t("Stop sharing"),
    );
    stop.addEventListener("click", () => {
      stop.disabled = true;
      void api
        .delete(`/api/shares/${share.id}`)
        .then(() => {
          showToast(t("Stopped sharing \"{name}\"", { name: share.name }));
          void load();
        })
        .catch((error: unknown) => {
          stop.disabled = false;
          if (!isHandledGlobally(error)) {
            errorBox.textContent = messageOf(error);
          }
        });
    });
    return h(
      "tr",
      { class: share.expired ? "disabled-row" : null },
      h(
        "td",
        {},
        h(
          "span",
          { class: "name-cell" },
          h("img", {
            class: "file-icon",
            src: iconUrl(share.kind === "folder" ? folderIconName(share.name) : fileIconName(share.name)),
            alt: "",
            width: 18,
            height: 18,
          }),
          h("span", { class: "item-name", title: share.name }, share.name),
          share.has_password ? h("span", { title: t("Requires a password") }, icon("lock", 14)) : null,
          share.one_time ? h("span", { class: "badge" }, t("One download")) : null,
        ),
      ),
      h("td", { class: "muted" }, [spaceLabel(share.space), ...parent].join(" / ")),
      h(
        "td",
        { class: "nowrap" },
        share.expired
          ? h("span", { class: "error-text" }, t("Expired"))
          : share.expires_at === null
            ? t("Never")
            : formatTime(share.expires_at),
      ),
      h("td", { class: "num" }, String(share.downloads)),
      h(
        "td",
        { class: "nowrap" },
        h(
          "div",
          { class: "share-actions" },
          share.expired
            ? null
            : h(
                "button",
                {
                  type: "button",
                  class: "link",
                  onClick: (event: Event) => void copyLink(shareUrl(share.token), event.currentTarget as Element),
                },
                t("Copy link"),
              ),
          stop,
        ),
      ),
    );
  };

  void load();
}

// ==================== 拿到連結的人 ====================

interface PublicInfo {
  readonly locked: boolean;
  readonly name: string | null;
  readonly kind: "file" | "folder" | null;
  readonly size: number | null;
  readonly owner: string | null;
  readonly expires_at: number | null;
  /** 只能下載一次 */
  readonly one_time: boolean;
}

interface PublicItem {
  readonly name: string;
  readonly kind: "file" | "folder";
  readonly size: number | null;
  readonly modified: number;
}

/** 網址是分享連結時回傳代碼：/s/{代碼} */
export function shareToken(): string | null {
  const match = /^\/s\/([A-Za-z0-9_-]{10,64})\/?$/.exec(window.location.pathname);
  return match?.[1] ?? null;
}

/** 分享的頁面。資料夾中的位置記在網址的 # 後面，重新整理或上一頁都會留在原處 */
export function renderPublicShare(container: HTMLElement, token: string): void {
  const base = `/api/public/shares/${encodeURIComponent(token)}`;
  const errorBox = h("p", { class: "error", role: "alert" });

  const fail = (error: unknown): void => {
    container.replaceChildren(
      h(
        "div",
        { class: "auth-form" },
        h("h1", {}, t("This link doesn't work")),
        h("p", {}, messageOf(error)),
      ),
    );
  };

  const subPath = (): string[] =>
    window.location.hash
      .replace(/^#\/?/, "")
      .split("/")
      .filter((part) => part !== "")
      .map((part) => {
        try {
          return decodeURIComponent(part);
        } catch {
          return "";
        }
      })
      .filter((part) => part !== "" && part !== "." && part !== "..");

  const show = async (): Promise<void> => {
    let info: PublicInfo;
    try {
      info = await api.get<PublicInfo>(base, { notifyUnauthorized: false });
    } catch (error) {
      fail(error);
      return;
    }
    if (info.locked) {
      askPassword();
      return;
    }
    const name = info.name ?? "";
    const owner = info.owner === null ? [] : [t("Shared by {name}", { name: info.owner })];
    const expires = info.expires_at === null ? [] : [t("Available until {date}", { date: formatTime(info.expires_at) })];
    const header = h(
      "div",
      { class: "share-head" },
      h("h1", { class: "share-title" }, name),
      h("p", { class: "muted" }, [...owner, ...expires].join(" · ")),
    );
    if (info.kind === "file") {
      container.replaceChildren(
        h(
          "div",
          { class: "share-page" },
          header,
          h(
            "div",
            { class: "share-file" },
            h("img", { class: "file-icon", src: iconUrl(fileIconName(name)), alt: "", width: 64, height: 64 }),
            h("p", {}, formatSize(info.size)),
            h(
              "a",
              { class: "button primary", href: `${base}/file`, download: name },
              icon("download", 16),
              t("Download"),
            ),
            info.one_time
              ? h(
                  "p",
                  { class: "hint" },
                  t("This link works for one download: it stops working as soon as the download starts. Keep this page open until the download finishes."),
                )
              : null,
          ),
        ),
      );
      return;
    }
    await showFolder(header, name);
  };

  const showFolder = async (header: HTMLElement, name: string): Promise<void> => {
    const path = subPath();
    const query = (extra: Record<string, string>): string => {
      const params = new URLSearchParams({ path: path.join("/"), ...extra }).toString();
      return params === "" ? "" : `?${params}`;
    };
    let items: PublicItem[];
    try {
      items = await api.get<PublicItem[]>(`${base}/items${query({})}`, { notifyUnauthorized: false });
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        askPassword();
        return;
      }
      fail(error);
      return;
    }
    const sorted = [...items].sort((a, b) =>
      a.kind !== b.kind ? (a.kind === "folder" ? -1 : 1) : a.name.localeCompare(b.name),
    );
    const crumbs = [name, ...path];
    const hrefOf = (parts: readonly string[]): string => `#/${parts.map(encodeURIComponent).join("/")}`;
    container.replaceChildren(
      h(
        "div",
        { class: "share-page" },
        header,
        h(
          "div",
          { class: "folder-header" },
          path.length === 0
            ? h("span", { class: "breadcrumbs" })
            : h(
            "nav",
            { class: "breadcrumbs", "aria-label": t("Current location") },
            h(
              "ol",
              {},
              crumbs.map((crumb, index) =>
                h(
                  "li",
                  {},
                  index === crumbs.length - 1
                    ? h("span", { "aria-current": "page" }, crumb)
                    : h("a", { href: hrefOf(path.slice(0, index)) }, crumb),
                ),
              ),
            ),
          ),
          h(
            "a",
            { class: "button secondary", href: `${base}/archive${query({})}`, download: "" },
            icon("download", 16),
            t("Download all (.zip)"),
          ),
        ),
        errorBox,
        sorted.length === 0
          ? h("p", { class: "empty-state" }, t("This folder is empty."))
          : h(
              "table",
              { class: "files public-files" },
              h(
                "thead",
                {},
                h(
                  "tr",
                  {},
                  h("th", { scope: "col" }, t("Name")),
                  h("th", { scope: "col", class: "num" }, t("Size")),
                  h("th", { scope: "col", class: "num" }, t("Modified")),
                ),
              ),
              h(
                "tbody",
                {},
                sorted.map((item) => {
                  const itemPath = [...path, item.name];
                  const href =
                    item.kind === "folder"
                      ? hrefOf(itemPath)
                      : `${base}/file?${new URLSearchParams({ path: itemPath.join("/") }).toString()}`;
                  return h(
                    "tr",
                    {},
                    h(
                      "td",
                      {},
                      h(
                        "a",
                        {
                          class: "name-cell",
                          href,
                          download: item.kind === "file" ? item.name : null,
                        },
                        h("img", {
                          class: "file-icon",
                          src: iconUrl(item.kind === "folder" ? folderIconName(item.name) : fileIconName(item.name)),
                          alt: "",
                          width: 18,
                          height: 18,
                        }),
                        h("span", { class: "item-name", title: item.name }, item.name),
                      ),
                    ),
                    h("td", { class: "num" }, formatSize(item.size)),
                    h("td", { class: "num" }, formatTime(item.modified)),
                  );
                }),
              ),
            ),
      ),
    );
  };

  const askPassword = (): void => {
    const input = h("input", {
      type: "password",
      class: "dialog-input",
      autocomplete: "off",
      "aria-label": t("Password"),
    });
    const error = h("p", { class: "error", role: "alert" });
    const submit = h("button", { type: "submit", class: "primary" }, t("Open"));
    const form = h(
      "form",
      { class: "auth-form" },
      h("h1", {}, t("This link is protected")),
      h("p", {}, t("Enter the password you were given to open it.")),
      h("label", {}, t("Password"), input),
      error,
      submit,
    );
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      submit.disabled = true;
      error.textContent = "";
      void api
        .post(`${base}/unlock`, { password: input.value }, { notifyUnauthorized: false })
        .then(() => show())
        .catch((e: unknown) => {
          submit.disabled = false;
          error.textContent = messageOf(e);
          input.select();
        });
    });
    container.replaceChildren(form);
    input.focus();
  };

  window.addEventListener("hashchange", () => void show());
  container.replaceChildren(h("p", { class: "muted" }, t("Loading…")));
  void show();
}
