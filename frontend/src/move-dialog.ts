// 選擇目的地資料夾的對話框，給「複製」與「移動」使用。
//
// 用瀏覽器內建的 <dialog>：showModal() 會把背景變成不能操作，Esc 自動關閉，
// 焦點也會留在對話框裡，不必自己處理這些細節。

import { t } from "./i18n.js";
import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { icon } from "./icons.js";
import { isInside } from "./listing.js";
import { apiUrl, joinPath, type Space } from "./space.js";
import type { Listing } from "./types.js";

/** 在哪一組檔案中選擇（見 space.ts） */
export interface FolderPlace {
  readonly space: Space;
  /** 路徑最前面的名稱：My files 或硬碟的名稱 */
  readonly rootLabel: string;
}

/** 選定的目的地 */
export interface FolderChoice {
  readonly place: FolderPlace;
  readonly path: readonly string[];
}

/**
 * 讓使用者選擇一個資料夾。
 *
 * @param start    一開始顯示的資料夾
 * @param excluded 不能選的資料夾：要移動的資料夾本身（不能搬進自己裡面）
 * @param place    一開始在哪一組檔案中選擇
 * @param places   可以選擇的每一組檔案（管理員：My files 與每一顆硬碟）；只有一組時不顯示選單
 * @returns 選定的資料夾；取消時為 undefined
 */
export function chooseFolder(
  title: string,
  confirmLabel: string,
  start: readonly string[],
  excluded: readonly (readonly string[])[],
  place: FolderPlace,
  places: readonly FolderPlace[] = [place],
): Promise<FolderChoice | undefined> {
  const origin = place;
  return new Promise((resolve) => {
    let location: readonly string[] = start;
    let result: FolderChoice | undefined;
    /** 載入序號：較慢回來的舊結果不能蓋掉新的 */
    let loading = 0;

    const path = h("nav", { class: "dialog-path", "aria-label": t("Location") });
    const list = h("ul", { class: "folder-list", "aria-label": t("Folders") });
    const error = h("p", { class: "error", role: "alert" });
    const note = h("p", { class: "hint" });
    const confirm = h("button", { type: "button", class: "primary" }, confirmLabel);
    const cancel = h("button", { type: "button", class: "secondary" }, t("Cancel"));
    const picker = h(
      "select",
      { class: "dialog-input", "aria-label": t("Disk") },
      places.map((option, index) =>
        h("option", { value: String(index), selected: option.space === place.space }, option.rootLabel),
      ),
    );
    picker.addEventListener("change", () => {
      place = places[Number(picker.value)] ?? origin;
      void open([]);
    });
    const dialog = h(
      "dialog",
      { class: "dialog", "aria-label": title },
      h("h2", { class: "dialog-title" }, title),
      ...(places.length > 1 ? [h("label", { class: "dialog-field" }, t("Disk"), picker)] : []),
      path,
      list,
      note,
      error,
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );

    cancel.addEventListener("click", () => dialog.close());
    confirm.addEventListener("click", () => {
      result = { place, path: location };
      dialog.close();
    });
    // 不論是按鈕、Esc 還是其他方式關閉，都經過這裡
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(result);
    });

    async function open(folder: readonly string[]): Promise<void> {
      const token = ++loading;
      location = folder;
      confirm.disabled = true;
      error.textContent = "";
      note.textContent = "";
      list.replaceChildren(h("li", {}, h("p", { class: "folder-empty" }, t("Loading…"))));
      renderPath();

      try {
        const listing = await api.get<Listing>(
          apiUrl("/api/files", place.space, { path: joinPath(folder) }),
        );
        if (token !== loading) {
          return;
        }
        renderFolders(listing.items.filter((item) => item.kind === "folder").map((item) => item.name));
        // 不能放進這裡（例如別人在 users 中的資料夾）：可以往下找，但不能選
        confirm.disabled = listing.read_only !== null;
        note.textContent = listing.read_only ?? "";
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
          failure instanceof ApiError ? failure.message : t("Couldn't load the folders");
      }
    }

    /** My files / 相片 / 2026：上層的每一層都可以點回去 */
    function renderPath(): void {
      const parts: Node[] = [pathStep(place.rootLabel, [], location.length === 0)];
      location.forEach((name, index) => {
        parts.push(h("span", { class: "separator", "aria-hidden": "true" }, "/"));
        parts.push(pathStep(name, location.slice(0, index + 1), index === location.length - 1));
      });
      path.replaceChildren(...parts);
    }

    function pathStep(name: string, target: readonly string[], isCurrent: boolean): Node {
      return isCurrent
        ? h("strong", { "aria-current": "location" }, name)
        : h("button", { type: "button", class: "link", onClick: () => void open(target) }, name);
    }

    function renderFolders(names: readonly string[]): void {
      if (names.length === 0) {
        list.replaceChildren(h("li", {}, h("p", { class: "folder-empty" }, t("No folders here"))));
        return;
      }
      const sorted = [...names].sort((a, b) => a.localeCompare(b, undefined, { numeric: true }));
      list.replaceChildren(
        ...sorted.map((name) => {
          const target = [...location, name];
          const blocked =
            place.space === origin.space && excluded.some((item) => isInside(target, item));
          return h(
            "li",
            {},
            h(
              "button",
              {
                type: "button",
                class: "folder-button",
                disabled: blocked,
                title: blocked ? t("A folder can't be moved into itself") : undefined,
                onClick: () => void open(target),
              },
              icon("folder"),
              name,
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
