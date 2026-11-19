// Administration → Storage：檔案可以放在哪些硬碟。
//
// 每個人的檔案放在其中一個位置，在 Users 的「⋯」選單中更換。
// 「Default」是設定中的資料目錄，永遠存在、不能刪除。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { confirmDialog } from "./dialogs.js";
import { h } from "./dom.js";
import { formatSize } from "./format.js";
import { icon } from "./icons.js";
import { showToast } from "./toast.js";
import type { StorageOverview, StorageVolume } from "./types.js";

/** 用量超過這個比例時，容量條以紅色顯示（與側邊欄相同） */
const ALMOST_FULL = 0.9;

export function renderStorage(container: HTMLElement): void {
  const errorBox = h("p", { class: "error", role: "alert" });
  const listSlot = h("div", {}, h("p", { class: "muted" }, "Loading…"));

  const reload = async (): Promise<void> => {
    try {
      const overview = await api.get<StorageOverview>("/api/admin/storage");
      listSlot.replaceChildren(volumeTable(overview.volumes, remove));
    } catch (error) {
      if (!isHandledGlobally(error)) {
        listSlot.replaceChildren(
          h("p", { class: "error", role: "alert" }, messageOf(error)),
        );
      }
    }
  };

  const remove = async (volume: StorageVolume): Promise<void> => {
    const confirmed = await confirmDialog({
      title: `Remove ${volume.name}?`,
      message:
        "ManorCloud will stop using this folder. Nothing on the disk is deleted, and nobody's files are there.",
      confirmLabel: "Remove",
      danger: true,
    });
    if (!confirmed) {
      return;
    }
    errorBox.textContent = "";
    try {
      await api.delete(`/api/admin/storage/${volume.id}`);
      showToast(`Removed ${volume.name}`);
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
      h("h2", { class: "section-title" }, "Storage"),
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
        "Add storage",
      ),
    ),
    h(
      "p",
      { class: "hint storage-intro" },
      "Each person's files are kept in one location. To put someone's files on another disk, ",
      "use Change storage in Users. ",
      "To use another disk or a NAS share, mount it on this computer first, then add it here.",
    ),
    errorBox,
    listSlot,
  );
  void reload();
}

function volumeTable(
  volumes: readonly StorageVolume[],
  remove: (volume: StorageVolume) => Promise<void>,
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
          h("th", { scope: "col" }, "Name"),
          h("th", { scope: "col" }, "Folder"),
          h("th", { scope: "col" }, "Space"),
          h("th", { scope: "col" }, "People"),
          h(
            "th",
            { scope: "col" },
            h("span", { class: "visually-hidden" }, "Actions"),
          ),
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
              volume.id === null
                ? h("span", { class: "muted" }, " (data directory)")
                : null,
            ),
            h("td", {}, h("code", { class: "path" }, volume.path)),
            h("td", { class: "space-cell" }, spaceMeter(volume)),
            h("td", {}, String(volume.users)),
            h("td", { class: "actions-cell" }, removeButton(volume, remove)),
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
      {
        class: "error",
        title: "The folder can't be read. Is the disk connected?",
      },
      "Not available",
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
        "aria-label": `${volume.name} usage`,
      },
      fill,
    ),
    h(
      "span",
      { class: "muted" },
      `${formatSize(volume.available_bytes)} free of ${formatSize(volume.total_bytes)}`,
    ),
  );
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
      title: inUse
        ? "Move everyone's files to another storage first"
        : `Remove ${volume.name}`,
      "aria-label": `Remove ${volume.name}`,
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
      placeholder: "Disk 2",
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
    const cancel = h(
      "button",
      { type: "button", class: "secondary" },
      "Cancel",
    );
    const confirm = h("button", { type: "submit", class: "primary" }, "Add");
    const title = "Add storage";
    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, title),
      h(
        "p",
        { class: "dialog-message" },
        "A folder on a disk connected to this server. It must already exist, and ManorCloud must be allowed to write to it.",
      ),
      h("label", { class: "dialog-field" }, "Name", name),
      h("label", { class: "dialog-field" }, "Folder", path),
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
        error.textContent = "Enter a name and a folder";
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
          showToast(`Added ${name.value.trim()}`);
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
  return error instanceof ApiError ? error.message : "Something went wrong";
}
