// 排序按鈕與選單（與 Google Drive 相同）：Sort by、Order、Folders 三個區塊。
// My files 與 Trash 共用；列表把它放在最右邊那一欄的標題（Grid 時放在方塊上方）。

import { t } from "./i18n.js";
import { h } from "./dom.js";
import { icon } from "./icons.js";
import { dropdown, menuSeparator } from "./menu.js";
import { defaultDirection, directionLabel, type SortKey, type SortOptions } from "./sort.js";

export interface SortControl {
  /** 按鈕與選單，交給 FileList 放置 */
  readonly element: HTMLElement;
  /** 排序改變後（例如點了欄位標題），更新選單中的勾選 */
  update(sort: SortOptions): void;
}

/**
 * @param modifiedLabel 時間那一欄的名稱：My files 是 Modified，Trash 是 Deleted
 */
export function sortControl(
  initial: SortOptions,
  onChange: (sort: SortOptions) => void,
  modifiedLabel = t("Modified"),
): SortControl {
  const menu = h("div", { class: "menu dropdown-menu", role: "menu", "aria-label": t("Sort") });
  const button = h(
    "button",
    { type: "button", class: "sort-menu-button", title: t("Sort") },
    icon("sort"),
    t("Sort"),
  );
  // 選單相對於視窗定位：放在可以左右捲動的列表裡，選單不會被裁切
  dropdown(button, menu, { fixed: true });

  const render = (sort: SortOptions): void => {
    const option = (label: string, checked: boolean, next: SortOptions): HTMLElement =>
      h(
        "button",
        {
          type: "button",
          role: "menuitemradio",
          "aria-checked": String(checked),
          class: "menu-item",
          onClick: () => onChange(next),
        },
        h("span", { class: "check-slot" }, checked ? icon("check", 16) : null),
        h("span", {}, label),
      );
    const section = (label: string): HTMLElement => h("div", { class: "menu-section" }, label);
    const keys: [SortKey, string][] = [
      ["name", t("Name")],
      ["modified", modifiedLabel],
      ["size", t("Size")],
    ];

    menu.replaceChildren(
      section(t("Sort by")),
      ...keys.map(([key, label]) =>
        option(
          label,
          sort.key === key,
          sort.key === key ? sort : { ...sort, key, direction: defaultDirection(key) },
        ),
      ),
      menuSeparator(),
      section(t("Order")),
      option(directionLabel(sort.key, "asc"), sort.direction === "asc", { ...sort, direction: "asc" }),
      option(directionLabel(sort.key, "desc"), sort.direction === "desc", {
        ...sort,
        direction: "desc",
      }),
      menuSeparator(),
      section(t("Folders")),
      option(t("On top"), sort.foldersFirst, { ...sort, foldersFirst: true }),
      option(t("Mixed with files"), !sort.foldersFirst, { ...sort, foldersFirst: false }),
    );
  };
  render(initial);

  return { element: h("div", { class: "sort-wrap" }, button, menu), update: render };
}
