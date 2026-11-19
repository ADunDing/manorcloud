// 畫面左下角的短暫通知，例如「3 items moved to trash · Undo」。
//
// 同一時間只顯示一則：新的通知取代舊的。幾秒後自動消失；滑鼠停在上面時暫停計時，
// 讓使用者來得及按 Undo。放在 document.body：畫面重新載入（renderBrowser）時不會跟著消失。

import { h } from "./dom.js";
import { icon } from "./icons.js";

/** 通知顯示多久 */
const DURATION_MS = 8000;

interface ToastAction {
  readonly label: string;
  readonly onClick: () => void;
}

let current: { element: HTMLElement; timer: number | undefined } | null = null;

export function showToast(message: string, action?: ToastAction): void {
  dismissToast();

  const close = h(
    "button",
    { type: "button", class: "toast-close", "aria-label": "Dismiss", title: "Dismiss" },
    icon("close", 16),
  );
  const element = h(
    "div",
    { class: "toast", role: "status" },
    h("span", { class: "toast-message" }, message),
    action === undefined
      ? null
      : h(
          "button",
          {
            type: "button",
            class: "toast-action",
            onClick: () => {
              dismissToast();
              action.onClick();
            },
          },
          action.label,
        ),
    close,
  );
  close.addEventListener("click", dismissToast);

  const state = { element, timer: undefined as number | undefined };
  const start = (): void => {
    state.timer = window.setTimeout(dismissToast, DURATION_MS);
  };
  element.addEventListener("mouseenter", () => window.clearTimeout(state.timer));
  element.addEventListener("mouseleave", start);

  document.body.append(element);
  current = state;
  start();
}

function dismissToast(): void {
  if (current === null) {
    return;
  }
  window.clearTimeout(current.timer);
  current.element.remove();
  current = null;
}
