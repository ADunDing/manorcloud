// 選擇目的地資料夾的對話框，給「移動」使用。
//
// 用瀏覽器內建的 <dialog>：showModal() 會把背景變成不能操作，Esc 自動關閉，
// 焦點也會留在對話框裡，不必自己處理這些細節。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { icon } from "./icons.js";
import { withSpace, type Space } from "./space.js";
import type { Crumb, FileItem, ItemDetail } from "./types.js";

/** 在哪一個檔案空間中選擇：自己的檔案，或管理員在另一顆硬碟上的檔案 */
export interface FolderPlace {
  readonly space: Space;
  /** 路徑最前面的名稱：My files 或硬碟的名稱 */
  readonly rootLabel: string;
}

/**
 * 讓使用者選擇一個資料夾。
 *
 * @param start    一開始顯示的資料夾（null 為 My files）
 * @param excluded 不能選的資料夾：要移動的資料夾本身（不能搬進自己裡面）
 * @param place    在哪一個檔案空間中選擇；只能選同一個空間中的資料夾
 * @returns 選定的資料夾編號（null 為 My files）；取消時為 undefined
 */
export function chooseFolder(
  title: string,
  confirmLabel: string,
  start: number | null,
  excluded: ReadonlySet<number>,
  place: FolderPlace = { space: null, rootLabel: "My files" },
): Promise<number | null | undefined> {
  return new Promise((resolve) => {
    let location: number | null = start;
    let result: number | null | undefined;
    /** 載入序號：較慢回來的舊結果不能蓋掉新的 */
    let loading = 0;

    const path = h("nav", { class: "dialog-path", "aria-label": "Location" });
    const list = h("ul", { class: "folder-list", "aria-label": "Folders" });
    const error = h("p", { class: "error", role: "alert" });
    const confirm = h("button", { type: "button", class: "primary" }, confirmLabel);
    const cancel = h("button", { type: "button", class: "secondary" }, "Cancel");
    const dialog = h(
      "dialog",
      { class: "dialog", "aria-label": title },
      h("h2", { class: "dialog-title" }, title),
      path,
      list,
      error,
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );

    cancel.addEventListener("click", () => dialog.close());
    confirm.addEventListener("click", () => {
      result = location;
      dialog.close();
    });
    // 不論是按鈕、Esc 還是其他方式關閉，都經過這裡
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(result);
    });

    async function open(folderId: number | null): Promise<void> {
      const token = ++loading;
      location = folderId;
      confirm.disabled = true;
      error.textContent = "";
      list.replaceChildren(h("li", {}, h("p", { class: "folder-empty" }, "Loading…")));

      try {
        const [items, detail] = await Promise.all([
          api.get<FileItem[]>(
            withSpace(
              folderId === null ? "/api/files" : `/api/files?parent_id=${folderId}`,
              place.space,
            ),
          ),
          folderId === null
            ? Promise.resolve(null)
            : api.get<ItemDetail>(withSpace(`/api/files/${folderId}`, place.space)),
        ]);
        if (token !== loading) {
          return;
        }
        renderPath(detail);
        renderFolders(items.filter((item) => item.kind === "folder"));
        confirm.disabled = false;
      } catch (failure) {
        if (token !== loading) {
          return;
        }
        if (isHandledGlobally(failure)) {
          dialog.close();
          return;
        }
        list.replaceChildren();
        error.textContent =
          failure instanceof ApiError ? failure.message : "Couldn't load the folders";
      }
    }

    /** My files / 相片 / 2026：上層的每一層都可以點回去 */
    function renderPath(detail: ItemDetail | null): void {
      const crumbs: Crumb[] = detail ? [...detail.ancestors, { id: detail.id, name: detail.name }] : [];
      const parts: Node[] = [pathStep(place.rootLabel, null, crumbs.length === 0)];
      crumbs.forEach((crumb, index) => {
        parts.push(h("span", { class: "separator", "aria-hidden": "true" }, "/"));
        parts.push(pathStep(crumb.name, crumb.id, index === crumbs.length - 1));
      });
      path.replaceChildren(...parts);
    }

    function pathStep(name: string, id: number | null, isCurrent: boolean): Node {
      return isCurrent
        ? h("strong", { "aria-current": "location" }, name)
        : h("button", { type: "button", class: "link", onClick: () => void open(id) }, name);
    }

    function renderFolders(folders: readonly FileItem[]): void {
      if (folders.length === 0) {
        list.replaceChildren(h("li", {}, h("p", { class: "folder-empty" }, "No folders here")));
        return;
      }
      list.replaceChildren(
        ...folders.map((folder) => {
          const blocked = excluded.has(folder.id);
          return h(
            "li",
            {},
            h(
              "button",
              {
                type: "button",
                class: "folder-button",
                disabled: blocked,
                title: blocked ? "A folder can't be moved into itself" : undefined,
                onClick: () => void open(folder.id),
              },
              icon("folder"),
              folder.name,
            ),
          );
        }),
      );
    }

    document.body.append(dialog);
    dialog.showModal();
    void open(start);
  });
}
