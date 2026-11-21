// 介面的語言：英文（原文）與繁體中文。
//
// - 畫面上的文字一律寫成英文並包在 t() 裡：t("Move here")。英文就是翻譯表的鍵，找不到翻譯時顯示英文
// - 會變的部分用 {名稱}：t("Moved {what} to {target}", { what, target })
// - 伺服器回傳的錯誤訊息也是英文，在 api.ts 用同一張翻譯表翻譯（內容有變動的訊息維持英文）
// - 語言存在這台裝置（localStorage）；沒有設定時跟隨瀏覽器的語言。更換後重新載入網頁
// - 翻譯表在 locales/；test/i18n.test.mjs 檢查每個 t("...") 都有翻譯

import { ZH_TW } from "./locales/zh-tw.js";

export type Language = "en" | "zh-TW";

/** 設定畫面的選項：auto 是跟隨瀏覽器 */
export type LanguageSetting = "auto" | Language;

const STORAGE_KEY = "manorcloud.language";

const DICTIONARIES: Readonly<Record<Language, Readonly<Record<string, string>> | null>> = {
  en: null,
  "zh-TW": ZH_TW,
};

function readSetting(): LanguageSetting {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY);
    if (saved === "en" || saved === "zh-TW") {
      return saved;
    }
  } catch {
    // 無痕模式等情況讀不到：跟隨瀏覽器
  }
  return "auto";
}

/** 瀏覽器的語言：中文（不分地區）用繁體中文，其他用英文 */
function browserLanguage(): Language {
  const languages = navigator.languages.length > 0 ? navigator.languages : [navigator.language];
  for (const language of languages) {
    const lower = language.toLowerCase();
    if (lower.startsWith("zh")) {
      return "zh-TW";
    }
    if (lower.startsWith("en")) {
      return "en";
    }
  }
  return "en";
}

const setting = readSetting();

/** 目前的語言（載入網頁時決定） */
export const language: Language = setting === "auto" ? browserLanguage() : setting;

const dictionary = DICTIONARIES[language];

document.documentElement.lang = language;

/** 目前的設定（設定畫面顯示用） */
export function languageSetting(): LanguageSetting {
  return setting;
}

/** 更換語言並重新載入網頁 */
export function setLanguage(value: LanguageSetting): void {
  try {
    if (value === "auto") {
      window.localStorage.removeItem(STORAGE_KEY);
    } else {
      window.localStorage.setItem(STORAGE_KEY, value);
    }
  } catch {
    // 存不了：只影響這一次
  }
  window.location.reload();
}

/** 翻譯。params 的值取代文字中的 {名稱} */
export function t(text: string, params?: Readonly<Record<string, string | number>>): string {
  const translated = dictionary !== null && Object.hasOwn(dictionary, text) ? (dictionary[text] ?? text) : text;
  if (params === undefined) {
    return translated;
  }
  return translated.replace(/\{(\w+)\}/g, (whole, name: string) =>
    Object.hasOwn(params, name) ? String(params[name]) : whole,
  );
}

/**
 * 翻譯含有元素的句子：{名稱} 換成元素（例如粗體的使用者名稱），回傳可以放進 h() 的片段。
 * 例：...tNodes("Type {text} to confirm", { text: h("strong", {}, expected) })
 */
export function tNodes(text: string, parts: Readonly<Record<string, Node | string>>): (Node | string)[] {
  const translated = t(text);
  const result: (Node | string)[] = [];
  let last = 0;
  for (const match of translated.matchAll(/\{(\w+)\}/g)) {
    const name = match[1] ?? "";
    if (!Object.hasOwn(parts, name)) {
      continue;
    }
    result.push(translated.slice(last, match.index));
    const part = parts[name];
    if (part !== undefined) {
      result.push(part);
    }
    last = match.index + match[0].length;
  }
  result.push(translated.slice(last));
  return result.filter((part) => part !== "");
}
