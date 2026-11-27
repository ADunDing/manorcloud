// Administration → Storage：檔案可以放在哪些硬碟。
//
// 每顆硬碟一個「人員」對話框，一個地方設定完所有事。最上方先選用途：
//   存放成員的個人檔案：每個人選「無法使用」或「個人檔案放這裡」（他的 My files 搬過來）
//   分享給成員：整顆硬碟出現在選定成員的側邊欄，各自「可檢視」或「可編輯」；沒有垃圾桶
// Default（資料目錄）永遠是存放個人檔案。
// 「Default」是設定中的資料目錄，永遠存在、不能刪除。

import { t } from "./i18n.js";
import { api, ApiError, isHandledGlobally } from "./api.js";
import { choice, confirmDialog } from "./dialogs.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { icon } from "./icons.js";
import { showToast } from "./toast.js";
import type { ManagedUser, StorageOverview, StorageVolume } from "./types.js";

/** 用量超過這個比例時，容量條以紅色顯示（與側邊欄相同） */
const ALMOST_FULL = 0.9;

export function renderStorage(container: HTMLElement): void {
  const errorBox = h("p", { class: "error", role: "alert" });
  const listSlot = h("div", {}, h("p", { class: "muted" }, t("Loading…")));

  const reload = async (): Promise<void> => {
    try {
      overview = await api.get<StorageOverview>("/api/admin/storage");
      listSlot.replaceChildren(volumeTable(overview.volumes, remove, setUp));
    } catch (error) {
      if (!isHandledGlobally(error)) {
        listSlot.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
    }
  };

  let overview: StorageOverview = { volumes: [], users: [], moves: [] };

  const setUp = async (volume: StorageVolume): Promise<void> => {
    errorBox.textContent = "";
    if (await peopleDialog(volume, overview)) {
      showToast(t("Saved"));
      await reload();
    }
  };

  const remove = async (volume: StorageVolume): Promise<void> => {
    const confirmed = await confirmDialog({
      title: t("Remove {name}?", { name: volume.name }),
      message:
        t("ManorCloud stops showing this storage, but the files on the disk stay where they are: add the same folder again to see them. Its shared links stop working, and items in its Trash can no longer be restored."),
      confirmLabel: t("Remove"),
      danger: true,
    });
    if (!confirmed) {
      return;
    }
    errorBox.textContent = "";
    try {
      await api.delete(`/api/admin/storage/${volume.id}`);
      showToast(t("Removed {name}", { name: volume.name }));
    } catch (error) {
      if (!isHandledGlobally(error)) {
        errorBox.textContent = messageOf(error);
      }
    }
    await reload();
  };

  container.replaceChildren(
    h(
      "div",
      { class: "admin-head" },
      h("h2", { class: "section-title" }, t("Storage")),
      h(
        "button",
        {
          type: "button",
          class: "primary",
          onClick: () =>
            void addLocationDialog().then((added) => {
              if (added) {
                void reload();
              }
            }),
        },
        icon("plus", 16),
        t("Add storage"),
      ),
    ),
    h(
      "p",
      { class: "hint storage-intro" },
      t(
        "Use the people button on a storage to choose whose files are kept there and who else can use it. To use another disk or a NAS share, mount it on this computer first, then add it here.",
      ),
    ),
    errorBox,
    listSlot,
    uploadLimitSection(),
    pendingUploadsSection(),
    webDavSection(),
  );
  void reload();
}

/** GET /api/admin/settings */
export interface ServerSettings {
  readonly max_upload_bytes: number;
  readonly default_max_upload_bytes: number;
  readonly custom_max_upload: boolean;
  readonly max_pending_uploads: number;
  readonly default_max_pending_uploads: number;
  readonly trusted_proxies: string;
  readonly connection_from: string;
  readonly webdav_enabled: boolean;
}

/** 以 GB 輸入：1024³ 位元組，與畫面上顯示的大小一致 */
const GB = 1024 ** 3;

/** 每人未完成的上傳數最多可以設定成多少，與伺服器相同 */
const MAX_PENDING_UPLOADS = 100;

/** 每個人最多幾個未完成的上傳 */
function pendingUploadsSection(): HTMLElement {
  const input = h("input", {
    type: "number",
    class: "dialog-input limit-input",
    min: "1",
    max: String(MAX_PENDING_UPLOADS),
    step: "1",
    "aria-label": t("Unfinished uploads per person"),
    disabled: true,
  });
  const save = h("button", { type: "submit", class: "primary", disabled: true }, t("Save"));
  const error = h("p", { class: "error", role: "alert" });

  const load = async (): Promise<void> => {
    try {
      const settings = await api.get<ServerSettings>("/api/admin/settings");
      input.value = String(settings.max_pending_uploads);
      input.disabled = false;
      save.disabled = false;
    } catch (e) {
      if (!isHandledGlobally(e)) {
        error.textContent = messageOf(e);
      }
    }
  };

  const form = h(
    "form",
    { class: "settings-group upload-limit" },
    h("h3", { class: "group-title" }, t("Unfinished uploads")),
    h(
      "p",
      { class: "hint" },
      t("How many interrupted uploads each person can keep, to continue later. Uploads with no progress for 24 hours are deleted automatically."),
    ),
    h("div", { class: "limit-row" }, input, save),
    error,
  );
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const count = Number(input.value);
    if (!Number.isInteger(count) || count < 1 || count > MAX_PENDING_UPLOADS) {
      error.textContent = t("Enter a number from 1 to {max}", { max: MAX_PENDING_UPLOADS });
      return;
    }
    error.textContent = "";
    save.disabled = true;
    void (async () => {
      try {
        await api.put("/api/admin/settings/pending-uploads", { max_pending_uploads: count });
        showToast(t("Saved"));
      } catch (e) {
        if (!isHandledGlobally(e)) {
          error.textContent = messageOf(e);
        }
      } finally {
        save.disabled = false;
      }
    })();
  });
  void load();
  return form;
}

/** 單一檔案的上傳上限：以 GB 輸入，或使用預設值 */
function uploadLimitSection(): HTMLElement {
  const input = h("input", {
    type: "number",
    class: "dialog-input limit-input",
    min: "0.001",
    step: "any",
    "aria-label": t("Upload limit in GB"),
    disabled: true,
  });
  const save = h("button", { type: "submit", class: "primary", disabled: true }, t("Save"));
  const reset = h("button", { type: "button", class: "secondary", hidden: true }, t("Use the default"));
  const hint = h("p", { class: "hint" });
  const error = h("p", { class: "error", role: "alert" });

  const show = (settings: ServerSettings): void => {
    input.value = String(Math.round((settings.max_upload_bytes / GB) * 1000) / 1000);
    input.disabled = false;
    save.disabled = false;
    reset.hidden = !settings.custom_max_upload;
    hint.textContent = settings.custom_max_upload
      ? t("The default is {size}.", { size: formatSize(settings.default_max_upload_bytes) })
      : t("This is the default.");
  };
  const load = async (): Promise<void> => {
    try {
      show(await api.get<ServerSettings>("/api/admin/settings"));
    } catch (e) {
      if (!isHandledGlobally(e)) {
        error.textContent = messageOf(e);
      }
    }
  };
  const update = async (bytes: number | null): Promise<void> => {
    error.textContent = "";
    save.disabled = true;
    try {
      await api.put("/api/admin/settings", { max_upload_bytes: bytes });
      showToast(t("Upload limit saved. It applies to uploads started from now on."));
      await load();
    } catch (e) {
      save.disabled = false;
      if (!isHandledGlobally(e)) {
        error.textContent = messageOf(e);
      }
    }
  };

  const form = h(
    "form",
    { class: "settings-group upload-limit" },
    h("h3", { class: "group-title" }, t("Upload limit")),
    h("p", { class: "hint" }, t("The largest single file anyone can upload.")),
    h("div", { class: "limit-row" }, input, h("span", {}, "GB"), save, reset),
    hint,
    error,
  );
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const gb = Number(input.value);
    if (!Number.isFinite(gb) || gb <= 0) {
      error.textContent = t("Enter a size in GB");
      return;
    }
    void update(Math.round(gb * GB));
  });
  reset.addEventListener("click", () => void update(null));
  void load();
  return form;
}

/** 是否開放 WebDAV：每個人用自己的應用程式密碼連線（Settings → WebDAV） */
function webDavSection(): HTMLElement {
  const enabled = h("input", { type: "checkbox", disabled: true });
  const error = h("p", { class: "error", role: "alert" });
  enabled.addEventListener("change", () => {
    error.textContent = "";
    enabled.disabled = true;
    const on = enabled.checked;
    void (async () => {
      try {
        await api.put("/api/admin/settings/webdav", { enabled: on });
        showToast(on ? t("WebDAV is on") : t("WebDAV is off"));
      } catch (e) {
        enabled.checked = !on;
        if (!isHandledGlobally(e)) {
          error.textContent = messageOf(e);
        }
      } finally {
        enabled.disabled = false;
      }
    })();
  });
  void (async () => {
    try {
      const settings = await api.get<ServerSettings>("/api/admin/settings");
      enabled.checked = settings.webdav_enabled;
      enabled.disabled = false;
    } catch (e) {
      if (!isHandledGlobally(e)) {
        error.textContent = messageOf(e);
      }
    }
  })();
  return h(
    "section",
    { class: "settings-group" },
    h("h3", { class: "group-title" }, t("WebDAV")),
    h("label", { class: "inline" }, enabled, t("Turn on WebDAV")),
    h(
      "p",
      { class: "hint" },
      t("People can open their own files as a network drive, or sync apps with them, at {address}. Each person signs in with an app password from Settings → WebDAV. The same rules apply as on the web: deleted items go to the trash, and storage and upload limits apply.", { address: `${window.location.origin}/dav/` }),
    ),
    error,
  );
}

function volumeTable(
  volumes: readonly StorageVolume[],
  remove: (volume: StorageVolume) => Promise<void>,
  setUp: (volume: StorageVolume) => Promise<void>,
): HTMLElement {
  return h(
    "div",
    { class: "users-wrap" },
    h(
      "table",
      { class: "users storage-table" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", { scope: "col" }, t("Name")),
          h("th", { scope: "col" }, t("Folder")),
          h("th", { scope: "col" }, t("Space")),
          h("th", { scope: "col" }, t("People")),
          h("th", { scope: "col" }, h("span", { class: "visually-hidden" }, t("Actions"))),
        ),
      ),
      h(
        "tbody",
        {},
        volumes.map((volume) =>
          h(
            "tr",
            {},
            h(
              "td",
              {},
              volume.name,
              volume.id === null ? h("span", { class: "muted" }, ` ${t("(data directory)")}`) : null,
              h(
                "div",
                { class: "hint" },
                volume.shared
                  ? volume.mounted === 1
                    ? t("Shared with 1 member")
                    : t("Shared with {count} members", { count: volume.mounted })
                  : t("Members' own files"),
              ),
            ),
            h("td", {}, h("code", { class: "path" }, volume.path)),
            h("td", { class: "space-cell" }, spaceMeter(volume)),
            h("td", {}, String(volume.users)),
            h("td", { class: "actions-cell" }, peopleButton(volume, setUp), removeButton(volume, remove)),
          ),
        ),
      ),
    ),
  );
}

/** 硬碟的用量：一條容量條與「120 GB free of 500 GB」 */
function spaceMeter(volume: StorageVolume): HTMLElement {
  if (volume.total_bytes === null || volume.available_bytes === null) {
    return h(
      "span",
      { class: "error", title: t("The folder can't be read. Is the disk connected?") },
      t("Not available"),
    );
  }
  const used = Math.max(0, volume.total_bytes - volume.available_bytes);
  const fraction = volume.total_bytes > 0 ? used / volume.total_bytes : 0;
  const fill = h("div", { class: "meter-fill" });
  fill.style.width = `${(fraction * 100).toFixed(1)}%`;
  return h(
    "div",
    { class: "space" },
    h(
      "div",
      {
        class: fraction >= ALMOST_FULL ? "meter almost-full" : "meter",
        role: "meter",
        "aria-valuemin": "0",
        "aria-valuemax": "100",
        "aria-valuenow": String(Math.round(fraction * 100)),
        "aria-label": t("{name} usage", { name: volume.name }),
      },
      fill,
    ),
    h(
      "span",
      { class: "muted" },
      t("{free} free of {total}", { free: formatSize(volume.available_bytes), total: formatSize(volume.total_bytes) }),
    ),
  );
}

/** 這顆硬碟的人員設定：誰的個人檔案放這裡、掛載給誰 */
function peopleButton(
  volume: StorageVolume,
  setUp: (volume: StorageVolume) => Promise<void>,
): HTMLElement {
  const label = t("Choose who uses {name}", { name: volume.name });
  return h(
    "button",
    { type: "button", class: "icon-button row-menu", title: label, "aria-label": label, onClick: () => void setUp(volume) },
    icon("users", 18),
  );
}

/** 每個人在這顆硬碟上的角色。view、edit 只在分享給成員時；home 只在存放個人檔案時 */
type Access = "none" | "view" | "edit" | "home";

/**
 * 這顆硬碟的人員。最上方先選用途，下面每個人的選項跟著用途改變：
 *
 * - 存放成員的個人檔案：每個人「無法使用」或「個人檔案放這裡」（他的 My files 搬過來）。
 *   個人檔案已經在這裡、或正在搬移的人不能在這裡改：要搬走，打開另一顆硬碟選「個人檔案放這裡」
 * - 分享給成員：每個成員「無法使用」「可檢視」或「可編輯」。管理員本來就看得到每一顆硬碟。
 *   還有人的個人檔案在這裡時不能改成分享
 *
 * Default（資料目錄）永遠是存放個人檔案，不顯示用途。
 * 儲存成功（至少一部分）時回傳 true；Cancel、Esc 回傳 false
 */
function peopleDialog(volume: StorageVolume, overview: StorageOverview): Promise<boolean> {
  return new Promise((resolve) => {
    let saved = false;
    const list = h("ul", { class: "mount-list" }, h("li", { class: "muted" }, t("Loading…")));
    const warning = h("p", { class: "error", role: "alert" });
    const error = h("p", { class: "error", role: "alert" });
    const cancel = h("button", { type: "button", class: "secondary" }, t("Cancel"));
    const save = h("button", { type: "submit", class: "primary", disabled: true }, t("Save"));
    const title = t("Who uses {name}", { name: volume.name });
    const canShare = volume.id !== null;

    const personalInput = h("input", { type: "radio", name: "purpose", value: "personal", checked: !volume.shared });
    const sharedInput = h("input", { type: "radio", name: "purpose", value: "shared", checked: volume.shared });
    const purpose = canShare
      ? h(
          "fieldset",
          { class: "choices" },
          h("legend", { class: "dialog-legend" }, t("Use this storage to")),
          choice(personalInput, t("Keep members' own files"), t("Each person sees only their own files: this is their My files.")),
          choice(
            sharedInput,
            t("Share with members"),
            t("Everyone you choose sees the files on this storage in their sidebar. It has no trash: deleting is permanent."),
          ),
        )
      : null;

    // 每個人在兩種用途下各自的選擇：切換用途再切回來時保留
    let users: ManagedUser[] = [];
    const personal = new Map<number, Access>();
    const shared = new Map<number, Access>();
    const homeOf = (user: ManagedUser): number | null =>
      overview.users.find((entry) => entry.user_id === user.id)?.volume_id ?? null;
    const isMoving = (user: ManagedUser): boolean => overview.moves.some((move) => move.user_id === user.id);
    // 個人檔案在這裡（或正在搬過來）的人：有他們時不能改成分享
    const keptHere = (): ManagedUser[] =>
      users.filter(
        (user) =>
          homeOf(user) === volume.id || overview.moves.some((m) => m.user_id === user.id && m.target_volume_id === volume.id),
      );
    const isShared = (): boolean => sharedInput.checked;

    const render = (): void => {
      warning.textContent = "";
      if (isShared()) {
        const kept = keptHere();
        if (kept.length > 0) {
          warning.textContent = t("{people}: their own files are kept on this storage. Keep them on another storage first.", {
            people: kept.map((user) => user.username).join(", "),
          });
        }
        const members = users.filter((user) => user.role === "member");
        list.replaceChildren(
          ...members.map((user) => row(user, shared, ["none", "view", "edit"], false)),
          h("li", { class: "muted" }, t("Administrators can already see every storage.")),
        );
        save.disabled = kept.length > 0;
      } else {
        list.replaceChildren(
          ...users.map((user) => {
            const locked = personal.get(user.id) === "home" && homeOf(user) === volume.id;
            return row(user, personal, ["none", "home"], locked || isMoving(user));
          }),
        );
        save.disabled = false;
      }
    };

    const row = (
      user: ManagedUser,
      choices: Map<number, Access>,
      values: readonly Access[],
      locked: boolean,
    ): HTMLElement => {
      const current = choices.get(user.id) ?? "none";
      const select = h(
        "select",
        {
          class: "dialog-input",
          "aria-label": t("{name} on this storage", { name: user.username }),
          disabled: locked,
          title: isMoving(user)
            ? t("Their files are being moved")
            : locked
              ? t("To move their files away, choose Keep their files here on another storage")
              : null,
        },
        values.map((value) => option(value, current)),
      );
      select.addEventListener("change", () => choices.set(user.id, select.value as Access));
      return h(
        "li",
        {},
        h(
          "span",
          { class: "mount-name" },
          user.username,
          user.role === "admin" ? h("span", { class: "muted" }, ` ${t("(administrator)")}`) : null,
        ),
        select,
      );
    };

    personalInput.addEventListener("change", render);
    sharedInput.addEventListener("change", render);

    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, title),
      purpose,
      warning,
      list,
      error,
      h("div", { class: "dialog-actions" }, cancel, save),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": title }, form);

    void (async () => {
      try {
        const [all, grants] = await Promise.all([
          api.get<ManagedUser[]>("/api/admin/users"),
          canShare
            ? api.get<{ user_id: number; writable: boolean }[]>(`/api/admin/storage/${volume.id}/mounts`)
            : Promise.resolve([]),
        ]);
        users = all;
        for (const user of users) {
          personal.set(user.id, homeOf(user) === volume.id ? "home" : "none");
          const grant = grants.find((g) => g.user_id === user.id);
          shared.set(user.id, grant === undefined ? "none" : grant.writable ? "edit" : "view");
        }
        render();
      } catch (e) {
        if (!isHandledGlobally(e)) {
          list.replaceChildren();
          error.textContent = messageOf(e);
        }
      }
    })();

    cancel.addEventListener("click", () => dialog.close());
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      save.disabled = true;
      error.textContent = "";
      void (async () => {
        const toShared = isShared();
        const purposeChanged = canShare && toShared !== volume.shared;
        const moving = toShared
          ? []
          : users.filter((user) => personal.get(user.id) === "home" && homeOf(user) !== volume.id);

        // 1. 確認：改變用途、搬移檔案
        if (purposeChanged) {
          const confirmed = await confirmDialog(
            toShared
              ? {
                  title: t("Share {name} with members?", { name: volume.name }),
                  message: t("It has no trash: files deleted there can't be restored. Its users folder, which only has what was left when people's files were moved away, is removed."),
                  confirmLabel: t("Share"),
                }
              : {
                  title: t("Stop sharing {name}?", { name: volume.name }),
                  message: t("Nobody it's shared with can open it any more. The files on the disk stay where they are."),
                  confirmLabel: t("Stop sharing"),
                  danger: true,
                },
          );
          if (!confirmed) {
            save.disabled = false;
            return;
          }
        }
        if (moving.length > 0) {
          const confirmed = await confirmDialog({
            title: t("Move files to {name}?", { name: volume.name }),
            message: t("{people}: their files are copied to {name} in the background. Until it's done they can browse and download, but not upload. Unfinished uploads are cancelled.", {
              people: moving.map((user) => user.username).join(", "),
              name: volume.name,
            }),
            confirmLabel: t("Move files"),
          });
          if (!confirmed) {
            save.disabled = false;
            return;
          }
        }

        // 2. 用途，然後分享的對象或搬移
        const failures: string[] = [];
        try {
          if (purposeChanged) {
            await api.put(`/api/admin/storage/${volume.id}/purpose`, { shared: toShared });
            saved = true;
          }
          if (toShared) {
            const grants = users
              .filter((user) => user.role === "member" && (shared.get(user.id) ?? "none") !== "none")
              .map((user) => ({ user_id: user.id, writable: shared.get(user.id) === "edit" }));
            await api.put(`/api/admin/storage/${volume.id}/mounts`, { grants });
            saved = true;
          }
          for (const user of moving) {
            try {
              await api.put(`/api/admin/users/${user.id}/storage`, { volume_id: volume.id });
              saved = true;
            } catch (e) {
              if (isHandledGlobally(e)) {
                return;
              }
              failures.push(`${user.username}: ${messageOf(e)}`);
            }
          }
        } catch (e) {
          if (!isHandledGlobally(e)) {
            error.textContent = messageOf(e);
          }
          save.disabled = false;
          return;
        }
        if (failures.length > 0) {
          error.textContent = failures.join(" · ");
          save.disabled = false;
          return;
        }
        dialog.close();
      })();
    });
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(saved);
    });

    document.body.append(dialog);
    dialog.showModal();
    cancel.focus();
  });
}

const ACCESS_LABELS: Readonly<Record<Access, () => string>> = {
  none: () => t("No access"),
  view: () => t("Can view"),
  edit: () => t("Can edit"),
  home: () => t("Keep their files here"),
};

function option(value: Access, current: Access): HTMLOptionElement {
  return h("option", { value, selected: value === current }, ACCESS_LABELS[value]());
}

function removeButton(
  volume: StorageVolume,
  remove: (volume: StorageVolume) => Promise<void>,
): HTMLElement | null {
  // 資料目錄不能刪除
  if (volume.id === null) {
    return null;
  }
  const inUse = volume.users > 0;
  return h(
    "button",
    {
      type: "button",
      class: "icon-button row-menu",
      disabled: inUse,
      title: inUse ? t("Move everyone's files to another storage first") : t("Remove {name}", { name: volume.name }),
      "aria-label": t("Remove {name}", { name: volume.name }),
      onClick: () => void remove(volume),
    },
    icon("trash", 18),
  );
}

/** 新增一個位置。成功時回傳 true */
function addLocationDialog(): Promise<boolean> {
  return new Promise((resolve) => {
    let added = false;
    const name = h("input", {
      type: "text",
      class: "dialog-input",
      maxlength: 40,
      placeholder: t("Disk 2"),
      autocomplete: "off",
    });
    const path = h("input", {
      type: "text",
      class: "dialog-input",
      placeholder: "/mnt/disk2/manorcloud",
      autocomplete: "off",
      autocapitalize: "none",
      spellcheck: "false",
    });
    const error = h("p", { class: "error", role: "alert" });
    const cancel = h("button", { type: "button", class: "secondary" }, t("Cancel"));
    const confirm = h("button", { type: "submit", class: "primary" }, t("Add"));
    const title = t("Add storage");
    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, title),
      h(
        "p",
        { class: "dialog-message" },
        t("A folder on a disk connected to this server. It must already exist, and ManorCloud must be allowed to write to it."),
      ),
      h("label", { class: "dialog-field" }, t("Name"), name),
      h("label", { class: "dialog-field" }, t("Folder"), path),
      error,
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": title }, form);

    cancel.addEventListener("click", () => dialog.close());
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(added);
    });
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (name.value.trim() === "" || path.value.trim() === "") {
        error.textContent = t("Enter a name and a folder");
        return;
      }
      confirm.disabled = true;
      error.textContent = "";
      void (async () => {
        try {
          await api.post("/api/admin/storage", {
            name: name.value.trim(),
            path: path.value.trim(),
          });
          added = true;
          showToast(t("Added {name}", { name: name.value.trim() }));
          dialog.close();
        } catch (failure) {
          if (isHandledGlobally(failure)) {
            dialog.close();
            return;
          }
          error.textContent = messageOf(failure);
          confirm.disabled = false;
        }
      })();
    });

    document.body.append(dialog);
    dialog.showModal();
    name.focus();
  });
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : t("Something went wrong");
}
