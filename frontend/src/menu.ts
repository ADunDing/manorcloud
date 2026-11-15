// 彈出式選單：頭像選單、New 選單與右鍵選單共用的規則。
//
// - 點選單以外的地方、按 Esc、視窗失去焦點或改變大小、選了一個項目：收起
// - 同一時間只開一個選單：打開新的會先關掉舊的
// - 方向鍵在項目之間移動
//
// Esc 在「捕獲階段」處理並呼叫 preventDefault：檔案列表的 Esc（取消選取）看到
// defaultPrevented 就不再處理，所以選單開著時按 Esc 只會關閉選單。

import { h } from "./dom.js";
import { icon, type IconName } from "./icons.js";

export interface MenuEntry {
  readonly label: string;
  readonly icon: IconName;
  readonly onSelect: () => void;
  readonly disabled?: boolean;
  /** 刪除等破壞性的動作，以紅色顯示 */
  readonly danger?: boolean;
  /** 右側顯示的快捷鍵，例如 F2 */
  readonly shortcut?: string;
  /** 停用時說明原因 */
  readonly title?: string;
}

/** 目前開著的選單的關閉函式 */
let closeCurrent: (() => void) | null = null;

/** 關閉目前開著的選單（如果有） */
export function closeMenus(): void {
  closeCurrent?.();
}

/** 選單中的一個項目 */
export function menuItem(entry: MenuEntry): HTMLButtonElement {
  return h(
    "button",
    {
      type: "button",
      role: "menuitem",
      class: entry.danger === true ? "menu-item danger" : "menu-item",
      disabled: entry.disabled ?? false,
      title: entry.title,
      onClick: entry.onSelect,
    },
    icon(entry.icon, 16),
    h("span", {}, entry.label),
    entry.shortcut === undefined
      ? null
      : h("kbd", { class: "menu-shortcut" }, entry.shortcut),
  );
}

/** 選單中的分隔線 */
export function menuSeparator(): HTMLElement {
  return h("div", { class: "menu-separator", role: "separator" });
}

/** 讓按鈕開關它的下拉選單 */
export function dropdown(button: HTMLButtonElement, menu: HTMLElement): void {
  button.setAttribute("aria-haspopup", "menu");
  button.setAttribute("aria-expanded", "false");
  menu.hidden = true;

  let close: (() => void) | null = null;
  button.addEventListener("click", () => {
    if (close !== null) {
      close();
      return;
    }
    menu.hidden = false;
    button.setAttribute("aria-expanded", "true");
    close = track(menu, button, () => {
      menu.hidden = true;
      button.setAttribute("aria-expanded", "false");
      close = null;
    });
    focusFirst(menu);
  });
}

/**
 * 在指定位置開啟右鍵選單。靠近視窗邊緣時往內移，不會超出畫面。
 *
 * @param title 選單最上方的說明，例如檔名或「3 items selected」
 */
export function openContextMenu(
  x: number,
  y: number,
  title: string,
  entries: readonly (MenuEntry | "separator")[],
): void {
  const menu = h(
    "div",
    { class: "menu context-menu", role: "menu", "aria-label": title },
    h("div", { class: "menu-title" }, title),
    entries.map((entry) => (entry === "separator" ? menuSeparator() : menuItem(entry))),
  );
  document.body.append(menu);

  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - rect.width - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - rect.height - 8))}px`;

  track(menu, null, () => menu.remove());
  focusFirst(menu);
}

/**
 * 開始追蹤一個開著的選單，回傳關閉它的函式。
 * 所有監聽都掛在同一個 AbortController 上，關閉時一次移除，不會留下殘餘的監聽。
 */
function track(
  menu: HTMLElement,
  anchor: HTMLElement | null,
  onClose: () => void,
): () => void {
  closeMenus();
  const controller = new AbortController();
  const { signal } = controller;

  const close = (): void => {
    if (closeCurrent !== close) {
      return;
    }
    closeCurrent = null;
    controller.abort();
    onClose();
  };
  closeCurrent = close;

  document.addEventListener(
    "mousedown",
    (event) => {
      const target = event.target;
      if (target instanceof Node && (menu.contains(target) || anchor?.contains(target))) {
        return;
      }
      close();
    },
    { capture: true, signal },
  );
  document.addEventListener(
    "keydown",
    (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        close();
        anchor?.focus();
      }
    },
    { capture: true, signal },
  );
  window.addEventListener("blur", close, { signal });
  window.addEventListener("resize", close, { signal });

  // 選了項目就收起。項目自己的 onClick 先執行（它掛在項目上，比這裡更早收到事件）
  menu.addEventListener(
    "click",
    (event) => {
      const target = event.target;
      if (target instanceof Element && target.closest('[role="menuitem"]:enabled') !== null) {
        close();
      }
    },
    { signal },
  );
  menu.addEventListener("keydown", (event) => moveFocus(menu, event), { signal });

  return close;
}

function enabledItems(menu: HTMLElement): HTMLElement[] {
  return Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]:enabled'));
}

function focusFirst(menu: HTMLElement): void {
  enabledItems(menu)[0]?.focus();
}

/** 上下鍵在可用的項目之間移動，頭尾相接 */
function moveFocus(menu: HTMLElement, event: KeyboardEvent): void {
  if (event.key !== "ArrowDown" && event.key !== "ArrowUp") {
    return;
  }
  event.preventDefault();
  const items = enabledItems(menu);
  if (items.length === 0) {
    return;
  }
  const index = items.indexOf(document.activeElement as HTMLElement);
  const step = event.key === "ArrowDown" ? 1 : -1;
  items[(index + step + items.length) % items.length]?.focus();
}
