// 網址安全檢查。獨立成一個不依賴 DOM 的模組，可以直接在 Node 上測試。

/** 允許出現在 href、src 等屬性中的網址協定 */
const ALLOWED_PROTOCOLS: ReadonlySet<string> = new Set([
  "http:",
  "https:",
  "blob:",
]);

/**
 * 網址是否安全：用允許清單，而不是禁止清單。
 *
 * 不自己比對字串：瀏覽器解析網址時會刪除其中的 Tab 與換行、去掉開頭的控制字元，
 * 所以 "java\tscript:alert(1)" 在瀏覽器眼中就是 "javascript:alert(1)"。
 * 交給 URL 解析器（與瀏覽器相同的 WHATWG 規範）解析後，檢查的就是瀏覽器最終會使用的結果。
 *
 * 允許清單的另一個好處：將來出現新的危險協定時，它預設就是被擋下的。
 *
 * @param value 要檢查的網址（可以是相對網址）
 * @param base  解析相對網址時的基準，瀏覽器中傳入 document.baseURI
 */
export function isSafeUrl(value: string, base: string): boolean {
  let url: URL;
  try {
    url = new URL(value, base);
  } catch {
    // 無法解析的網址一律視為不安全
    return false;
  }
  return ALLOWED_PROTOCOLS.has(url.protocol);
}
