// 前端原始碼不得使用會把字串當成 HTML 或程式碼執行的 API（見 src/dom.ts 的鐵律）。
// 這些是 XSS 最常見的入口：只要有一處把使用者的資料交給它們，就能在別人的瀏覽器執行程式。
//
// 檢查的是原始碼（src/），不是編譯後的輸出。註解中提到這些名稱不算違規。

import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const SRC = new URL("../src/", import.meta.url).pathname;

const FORBIDDEN = [
  [/\.innerHTML\b/, "innerHTML"],
  [/\.outerHTML\b/, "outerHTML"],
  [/\binsertAdjacentHTML\b/, "insertAdjacentHTML"],
  [/\bdocument\.write(ln)?\s*\(/, "document.write"],
  [/\bcreateContextualFragment\b/, "createContextualFragment"],
  [/\bDOMParser\b/, "DOMParser"],
  [/(^|[^.\w])eval\s*\(/, "eval"],
  [/\bnew\s+Function\s*\(/, "new Function"],
  [/\bset(Timeout|Interval)\s*\(\s*["'`]/, "setTimeout/setInterval with a string"],
];

/** 所有 .ts 檔案（含子資料夾） */
function sources(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      return sources(path);
    }
    return entry.name.endsWith(".ts") ? [path] : [];
  });
}

/** 去掉註解，只留下程式碼：說明文字裡提到這些名稱不算違規 */
function withoutComments(code) {
  return code.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:"'`])\/\/.*$/gm, "$1");
}

test("前端不使用會把字串當成 HTML 或程式碼的 API", () => {
  const found = [];
  for (const file of sources(SRC)) {
    const lines = withoutComments(readFileSync(file, "utf8")).split("\n");
    lines.forEach((line, index) => {
      for (const [pattern, name] of FORBIDDEN) {
        if (pattern.test(line)) {
          found.push(`${file.slice(SRC.length)}:${index + 1} uses ${name}`);
        }
      }
    });
  }
  assert.deepEqual(found, []);
});

test("檢查本身有效：違規的寫法會被找到", () => {
  const code = withoutComments("el.innerHTML = name;\n// el.innerHTML 只是註解\nconst x = 1;");
  const hits = FORBIDDEN.filter(([pattern]) => pattern.test(code)).map(([, name]) => name);
  assert.deepEqual(hits, ["innerHTML"]);
});
