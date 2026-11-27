// Administration → Storage：檔案可以放在哪些硬碟。
//
// 每個人的檔案放在其中一個位置，在 Users 的「⋯」選單中更換。
// 「Default」是設定中的資料目錄，永遠存在、不能刪除。
// 其他位置可以整個掛載給一般使用者（人像按鈕），出現在他們的側邊欄。

import { t } from "./i18n.js";
import { api, ApiError, isHandledGlobally } from "./api.js";
import { confirmDialog } from "./dialogs.js";
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
      const overview = await api.get<StorageOverview>("/api/admin/storage");
      listSlot.replaceChildren(volumeTable(overview.volumes, remove, share));
    } catch (error) {
      if (!isHandledGlobally(error)) {
        listSlot.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
    }
  };

  const share = async (volume: StorageVolume): Promise<void> => {
    errorBox.textContent = "";
    if (await mountDialog(volume)) {
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
        "Each person's files are kept in one location. To put someone's files on another disk, use Change storage in Users. To use another disk or a NAS share, mount it on this computer first, then add it here.",
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
  share: (volume: StorageVolume) => Promise<void>,
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
              volume.mounted > 0
                ? h(
                    "div",
                    { class: "hint" },
                    volume.mounted === 1
                      ? t("Mounted for 1 person")
                      : t("Mounted for {count} people", { count: volume.mounted }),
                  )
                : null,
            ),
            h("td", {}, h("code", { class: "path" }, volume.path)),
            h("td", { class: "space-cell" }, spaceMeter(volume)),
            h("td", {}, String(volume.users)),
            h("td", { class: "actions-cell" }, shareButton(volume, share), removeButton(volume, remove)),
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

/** 掛載給使用者。有人的 My files 在這裡時不能掛載：那裡的 users 資料夾是他們的私人檔案 */
function shareButton(
  volume: StorageVolume,
  share: (volume: StorageVolume) => Promise<void>,
): HTMLElement | null {
  if (volume.id === null) {
    return null;
  }
  const hasPeople = volume.users > 0;
  const label = t("Choose who can use {name}", { name: volume.name });
  return h(
    "button",
    {
      type: "button",
      class: "icon-button row-menu",
      disabled: hasPeople,
      title: hasPeople ? t("Someone's own files are on this storage, so it can't be mounted for people") : label,
      "aria-label": label,
      onClick: () => void share(volume),
    },
    icon("users", 18),
  );
}

type Access = "none" | "read" | "write";

/**
 * 選擇這個位置掛載給哪些一般使用者，各自唯讀或可以修改。
 * 儲存成功時回傳 true；Cancel、Esc 回傳 false
 */
function mountDialog(volume: StorageVolume): Promise<boolean> {
  return new Promise((resolve) => {
    let saved = false;
    const list = h("ul", { class: "mount-list" }, h("li", { class: "muted" }, t("Loading…")));
    const error = h("p", { class: "error", role: "alert" });
    const cancel = h("button", { type: "button", class: "secondary" }, t("Cancel"));
    const save = h("button", { type: "submit", class: "primary", disabled: true }, t("Save"));
    const title = t("Who can use {name}", { name: volume.name });
    const choices: { readonly userId: number; readonly select: HTMLSelectElement }[] = [];

    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, title),
      h(
        "p",
        { class: "dialog-message" },
        t("The whole storage appears in their sidebar. Files there are shared by everyone you choose and don't count toward their storage limit."),
      ),
      list,
      error,
      h("div", { class: "dialog-actions" }, cancel, save),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": title }, form);

    void (async () => {
      try {
        const [users, grants] = await Promise.all([
          api.get<ManagedUser[]>("/api/admin/users"),
          api.get<{ user_id: number; writable: boolean }[]>(`/api/admin/storage/${volume.id}/mounts`),
        ]);
        const members = users.filter((user) => user.role === "member");
        if (members.length === 0) {
          list.replaceChildren(h("li", { class: "muted" }, t("There are no members yet. Add people in Users first.")));
          return;
        }
        list.replaceChildren(
          ...members.map((user) => {
            const grant = grants.find((g) => g.user_id === user.id);
            const current: Access = grant === undefined ? "none" : grant.writable ? "write" : "read";
            const select = h(
              "select",
              { class: "dialog-input", "aria-label": t("Access for {name}", { name: user.username }) },
              option("none", t("No access"), current),
              option("read", t("Read only"), current),
              option("write", t("Read and write"), current),
            );
            choices.push({ userId: user.id, select });
            return h("li", {}, h("span", { class: "mount-name" }, user.username), select);
          }),
        );
        save.disabled = false;
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
      const grants = choices
        .filter((choice) => choice.select.value !== "none")
        .map((choice) => ({ user_id: choice.userId, writable: choice.select.value === "write" }));
      void (async () => {
        try {
          await api.put(`/api/admin/storage/${volume.id}/mounts`, { grants });
          saved = true;
          dialog.close();
        } catch (e) {
          if (!isHandledGlobally(e)) {
            error.textContent = messageOf(e);
          }
          save.disabled = false;
        }
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

function option(value: Access, label: string, current: Access): HTMLOptionElement {
  return h("option", { value, selected: value === current }, label);
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
