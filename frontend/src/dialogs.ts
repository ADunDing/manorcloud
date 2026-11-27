// 對話框：確認、輸入名稱（New folder）、選擇下載方式與格式。
// 整個程式都用這裡的對話框，不用瀏覽器內建的 confirm：外觀一致，也跟著深色模式。
//
// 用瀏覽器內建的 <dialog>：showModal() 讓背景不能操作，Esc 自動關閉，焦點留在對話框裡。

import { t, tNodes } from "./i18n.js";
import { ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { icon } from "./icons.js";

interface ConfirmOptions {
  readonly title: string;
  readonly message: string;
  readonly confirmLabel: string;
  /** 刪除等無法復原的動作：確認按鈕以紅色顯示，預設的焦點放在 Cancel */
  readonly danger?: boolean;
  /**
   * 後果特別嚴重的動作（例如刪除使用者與他所有的檔案）：
   * 必須輸入這段文字，確認按鈕才能按下
   */
  readonly typeToConfirm?: string;
}

/** 確認一個動作。按下確認回傳 true；Cancel、Esc 回傳 false */
export function confirmDialog(options: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => {
    let confirmed = false;
    const cancel = h(
      "button",
      { type: "button", class: "secondary" },
      t("Cancel"),
    );
    const confirm = h(
      "button",
      {
        type: "submit",
        class: options.danger === true ? "primary danger" : "primary",
      },
      options.confirmLabel,
    );

    const expected = options.typeToConfirm;
    let typed: HTMLInputElement | null = null;
    let typedField: HTMLElement | null = null;
    if (expected !== undefined) {
      typed = h("input", {
        type: "text",
        class: "dialog-input",
        autocomplete: "off",
        autocapitalize: "none",
        spellcheck: "false",
      });
      const input = typed;
      confirm.disabled = true;
      input.addEventListener("input", () => {
        confirm.disabled = input.value !== expected;
      });
      typedField = h(
        "label",
        { class: "dialog-type-confirm" },
        h("span", {}, tNodes("Type {text} to confirm", { text: h("strong", {}, expected) })),
        input,
      );
    }

    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, options.title),
      // 文字是純文字（textContent），檔名或帳號名稱不會被當成 HTML 解析
      h("p", { class: "dialog-message" }, options.message),
      typedField,
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h(
      "dialog",
      { class: "dialog", "aria-label": options.title },
      form,
    );

    cancel.addEventListener("click", () => dialog.close());
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      // 輸入欄中按 Enter 也會送出：文字不符時不算確認
      if (expected !== undefined && typed?.value !== expected) {
        return;
      }
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
    (typed ?? (options.danger === true ? cancel : confirm)).focus();
  });
}

interface NameDialogOptions {
  readonly title: string;
  readonly message: string;
  readonly confirmLabel: string;
  /** 顯示在輸入欄下方的位置，例如 ["My files", "相片", "2026"]。不是檔案時省略 */
  readonly location?: readonly string[];
  readonly initial?: string;
  /** 放在輸入欄下方的其他欄位（例如建立使用者時選擇硬碟） */
  readonly extra?: HTMLElement;
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
    const cancel = h(
      "button",
      { type: "button", class: "secondary" },
      t("Cancel"),
    );
    const confirm = h(
      "button",
      { type: "submit", class: "primary" },
      options.confirmLabel,
    );
    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, options.title),
      h("p", { class: "dialog-message" }, options.message),
      input,
      options.extra ?? null,
      options.location === undefined
        ? null
        : h(
            "p",
            { class: "dialog-location" },
            icon("folder", 16),
            options.location.join(" / "),
          ),
      error,
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h(
      "dialog",
      { class: "dialog", "aria-label": options.title },
      form,
    );

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
        error.textContent = t("Enter a name");
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
            failure instanceof ApiError
              ? failure.message
              : t("Something went wrong");
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

/** 打包下載的格式：值就是副檔名，也是 /api/archive 的 format 參數 */
export type ArchiveFormat = "zip" | "tar" | "tar.gz" | "tar.zst";

export type DownloadChoice = ArchiveFormat | "separate";

/**
 * 「個別下載」這個選項：
 * - hidden：只有一個項目（例如一個資料夾），不顯示
 * - available：可以選
 * - folders：選了資料夾，不能選（瀏覽器無法直接下載一整個資料夾）
 */
type SeparateOption = "hidden" | "available" | "folders";

const FORMATS: readonly { readonly format: ArchiveFormat; readonly hint: () => string }[] = [
  { format: "zip", hint: () => t("Opens on any computer. Packed, not compressed") },
  { format: "tar", hint: () => t("For Linux and NAS. Packed, not compressed") },
  { format: "tar.gz", hint: () => t("Compressed. Makes documents smaller, but not photos or videos") },
  { format: "tar.zst", hint: () => t("Compressed like .tar.gz, but faster. Needs a recent system") },
];

const FORMAT_KEY = "manorcloud.archive-format";

/** 上次選的格式：下次預先選好。沒有記錄或無法使用 localStorage 時是 zip */
function lastFormat(): ArchiveFormat {
  try {
    const saved = localStorage.getItem(FORMAT_KEY);
    return FORMATS.find((f) => f.format === saved)?.format ?? "zip";
  } catch {
    return "zip";
  }
}

function saveFormat(format: ArchiveFormat): void {
  try {
    localStorage.setItem(FORMAT_KEY, format);
  } catch {
    // 無法儲存只代表下次預先選的是 zip
  }
}

/**
 * 選擇下載方式：打包成哪一種格式，或（選了多個檔案時）個別下載。
 * Cancel、Esc 回傳 undefined
 */
export function chooseDownload(
  title: string,
  separate: SeparateOption,
): Promise<DownloadChoice | undefined> {
  return new Promise((resolve) => {
    let result: DownloadChoice | undefined;
    const selected = lastFormat();

    const options = FORMATS.map(({ format, hint }) => ({
      format,
      hint,
      input: h("input", {
        type: "radio",
        name: "download-mode",
        value: format,
        checked: format === selected,
      }),
    }));
    const separateInput = h("input", {
      type: "radio",
      name: "download-mode",
      value: "separate",
      disabled: separate === "folders",
    });
    const cancel = h(
      "button",
      { type: "button", class: "secondary" },
      t("Cancel"),
    );
    const confirm = h(
      "button",
      { type: "submit", class: "primary" },
      t("Download"),
    );

    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, title),
      h(
        "fieldset",
        { class: "choices" },
        h("legend", { class: "visually-hidden" }, t("How to download")),
        options.map(({ format, hint, input }) => choice(input, `.${format}`, hint())),
        separate === "hidden"
          ? null
          : choice(
              separateInput,
              t("As separate files"),
              separate === "folders"
                ? t("Not available when folders are selected")
                : t("Your browser may ask you to allow several downloads"),
            ),
      ),
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": title }, form);

    cancel.addEventListener("click", () => dialog.close());
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const picked = options.find((option) => option.input.checked)?.format;
      if (picked !== undefined) {
        saveFormat(picked);
      }
      result = picked ?? "separate";
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

/** 只選格式（沒有「個別下載」）：下載一個資料夾、分享的資料夾時使用 */
export async function chooseArchiveFormat(title: string): Promise<ArchiveFormat | undefined> {
  const choice = await chooseDownload(title, "hidden");
  return choice === "separate" ? undefined : choice;
}

/** 單選的一個選項：粗體的名稱，下面一行說明 */
export function choice(
  input: HTMLInputElement,
  label: string,
  hint: string,
): HTMLElement {
  return h(
    "label",
    { class: input.disabled ? "choice disabled" : "choice" },
    input,
    h(
      "span",
      { class: "choice-text" },
      h("strong", {}, label),
      h("span", { class: "hint" }, hint),
    ),
  );
}
