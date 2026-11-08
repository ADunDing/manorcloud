// 建立 DOM 元素的唯一入口。
//
// 第一條鐵律：使用者提供的資料絕不當成 HTML 解析。
// 這個模組從結構上保證這件事：字串子節點一律變成文字節點，
// 整個前端只要都用 h() 建立畫面，就不需要在每個地方記得「這裡要用 textContent」。
// 請勿在其他地方使用 innerHTML、outerHTML、insertAdjacentHTML。

import { isSafeUrl } from "./url.js";

/** 子節點：字串與數字變成文字節點；陣列會被展開；null、undefined、false 被忽略 */
export type Child =
  | Node
  | string
  | number
  | null
  | undefined
  | false
  | readonly Child[];

export type EventHandler = (event: Event) => void;

/** 屬性值：`onXxx` 必須是函式，其他屬性不能是函式 */
export type PropValue =
  | string
  | number
  | boolean
  | null
  | undefined
  | EventHandler;

export type Props = Readonly<Record<string, PropValue>>;

/** 值是網址的屬性：只接受 http、https、blob */
const URL_ATTRIBUTES: ReadonlySet<string> = new Set([
  "href",
  "src",
  "action",
  "formaction",
]);

/** 會把字串當成 HTML 解析的屬性，一律禁止 */
const FORBIDDEN_ATTRIBUTES: ReadonlySet<string> = new Set(["srcdoc"]);

/**
 * 建立一個元素。
 *
 * @example
 * h("p", { class: "note" }, "檔名：", h("strong", {}, file.name))
 */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Props = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);

  for (const [key, value] of Object.entries(props)) {
    if (value === null || value === undefined || value === false) {
      continue;
    }

    const name = key.toLowerCase();

    if (name.startsWith("on")) {
      // 事件只能是函式。傳入字串（例如 onclick: "..."）會被 CSP 擋下而默默失效，
      // 與其讓按鈕點了沒反應，不如在開發時就報錯。
      if (typeof value !== "function") {
        throw new TypeError(`事件屬性 ${key} 必須是函式`);
      }
      el.addEventListener(name.slice(2), value);
      continue;
    }

    if (typeof value === "function") {
      throw new TypeError(`${key} 不是事件屬性，不能傳入函式`);
    }
    if (FORBIDDEN_ATTRIBUTES.has(name)) {
      throw new TypeError(`不可設定 ${key} 屬性`);
    }
    if (name === "class") {
      el.className = String(value);
      continue;
    }

    const text = value === true ? "" : String(value);
    // CSP 也會擋下 javascript: 網址，這裡是第二道防線
    if (URL_ATTRIBUTES.has(name) && !isSafeUrl(text, document.baseURI)) {
      throw new TypeError(`${key} 只接受 http、https 或 blob 網址`);
    }
    el.setAttribute(key, text);
  }

  appendChildren(el, children);
  return el;
}

function appendChildren(parent: Node, children: readonly Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) {
      continue;
    }
    if (Array.isArray(child)) {
      appendChildren(parent, child);
    } else if (child instanceof Node) {
      parent.appendChild(child);
    } else {
      parent.appendChild(document.createTextNode(String(child)));
    }
  }
}
