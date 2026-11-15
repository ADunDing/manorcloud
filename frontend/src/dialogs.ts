// 對話框：確認、輸入名稱（New folder）、選擇下載方式。
// 整個程式都用這裡的對話框，不用瀏覽器內建的 confirm：外觀一致，也跟著深色模式。
//
// 用瀏覽器內建的 <dialog>：showModal() 讓背景不能操作，Esc 自動關閉，焦點留在對話框裡。

import { ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { icon } from "./icons.js";

export interface ConfirmOptions {
  readonly title: string;
  readonly message: string;
  readonly confirmLabel: string;
  /** 刪除等無法復原的動作：確認按鈕以紅色顯示，預設的焦點放在 Cancel */
  readonly danger?: boolean;
}

/** 確認一個動作。按下確認回傳 true；Cancel、Esc 回傳 false */
export function confirmDialog(options: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => {
    let confirmed = false;
    const cancel = h("button", { type: "button", class: "secondary" }, "Cancel");
    const confirm = h(
      "button",
      { type: "submit", class: options.danger === true ? "primary danger" : "primary" },
      options.confirmLabel,
    );
    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, options.title),
      // 文字是純文字（textContent），檔名或帳號名稱不會被當成 HTML 解析
      h("p", { class: "dialog-message" }, options.message),
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": options.title }, form);

    cancel.addEventListener("click", () => dialog.close());
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      confirmed = true;
      dialog.close();
    });
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(confirmed);
    });

    document.body.append(dialog);
    dialog.showModal();
    // 無法復原的動作：按 Enter 不會不小心確認
    (options.danger === true ? cancel : confirm).focus();
  });
}

export interface NameDialogOptions {
  readonly title: string;
  readonly message: string;
  readonly confirmLabel: string;
  /** 顯示在輸入欄下方的位置，例如 ["My files", "相片", "2026"] */
  readonly location: readonly string[];
  readonly initial?: string;
  /**
   * 按下確認時呼叫。拋出錯誤時，錯誤訊息顯示在對話框裡，對話框不關閉，
   * 使用者改好名稱再按一次就好（例如同名項目已存在）
   */
  readonly submit: (name: string) => Promise<void>;
}

/** 輸入一個名稱。成功送出時回傳 true，取消時回傳 false */
export function nameDialog(options: NameDialogOptions): Promise<boolean> {
  return new Promise((resolve) => {
    let succeeded = false;

    const input = h("input", {
      type: "text",
      class: "dialog-input",
      value: options.initial ?? "",
      "aria-label": options.title,
      autocomplete: "off",
      spellcheck: "false",
    });
    const error = h("p", { class: "error", role: "alert" });
    const cancel = h("button", { type: "button", class: "secondary" }, "Cancel");
    const confirm = h("button", { type: "submit", class: "primary" }, options.confirmLabel);
    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, options.title),
      h("p", { class: "dialog-message" }, options.message),
      input,
      h("p", { class: "dialog-location" }, icon("folder", 16), options.location.join(" / ")),
      error,
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": options.title }, form);

    cancel.addEventListener("click", () => dialog.close());
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(succeeded);
    });

    form.addEventListener("submit", (event) => {
      // 自己決定什麼時候關閉：失敗時要留著對話框
      event.preventDefault();
      const name = input.value.trim();
      if (name === "") {
        error.textContent = "Enter a name";
        input.focus();
        return;
      }

      input.disabled = true;
      confirm.disabled = true;
      error.textContent = "";
      void (async () => {
        try {
          await options.submit(name);
          succeeded = true;
          dialog.close();
        } catch (failure) {
          // 登入失效等：畫面已經換掉，對話框也跟著關閉
          if (isHandledGlobally(failure)) {
            dialog.close();
            return;
          }
          error.textContent =
            failure instanceof ApiError ? failure.message : "Something went wrong";
          input.disabled = false;
          confirm.disabled = false;
          input.focus();
          input.select();
        }
      })();
    });

    document.body.append(dialog);
    dialog.showModal();
    input.focus();
    input.select();
  });
}

export type DownloadChoice = "zip" | "separate";

/**
 * 選了多個項目時，選擇下載成一個 .zip 還是分開下載。
 * 選了資料夾時只能用 .zip：瀏覽器無法直接下載一整個資料夾。
 */
export function chooseDownload(
  count: number,
  hasFolders: boolean,
): Promise<DownloadChoice | undefined> {
  return new Promise((resolve) => {
    let result: DownloadChoice | undefined;

    const zip = h("input", { type: "radio", name: "download-mode", value: "zip", checked: true });
    const separate = h("input", {
      type: "radio",
      name: "download-mode",
      value: "separate",
      disabled: hasFolders,
    });
    const cancel = h("button", { type: "button", class: "secondary" }, "Cancel");
    const confirm = h("button", { type: "submit", class: "primary" }, "Download");
    const title = `Download ${count} items`;

    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, title),
      h(
        "fieldset",
        { class: "choices" },
        h("legend", { class: "visually-hidden" }, "How to download"),
        choice(zip, "As one .zip file", "The files are packed together, not compressed"),
        choice(
          separate,
          "As separate files",
          hasFolders
            ? "Not available when folders are selected"
            : "Your browser may ask you to allow several downloads",
        ),
      ),
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": title }, form);

    cancel.addEventListener("click", () => dialog.close());
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      result = separate.checked ? "separate" : "zip";
      dialog.close();
    });
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(result);
    });

    document.body.append(dialog);
    dialog.showModal();
    confirm.focus();
  });
}

function choice(input: HTMLInputElement, label: string, hint: string): HTMLElement {
  return h(
    "label",
    { class: input.disabled ? "choice disabled" : "choice" },
    input,
    h("span", { class: "choice-text" }, h("strong", {}, label), h("span", { class: "hint" }, hint)),
  );
}
