// 網址安全檢查的測試，使用 Node 內建的測試工具，不需要額外的套件。
// Node 的 URL 實作與瀏覽器相同（WHATWG URL 規範），解析結果一致。
// 測試的是 tsc 編譯後的輸出，所以 `npm test` 會先執行 tsc。

import { test } from "node:test";
import assert from "node:assert/strict";
import { isSafeUrl } from "../public/js/url.js";

const BASE = "http://localhost:3000/";

test("允許本站的相對網址與一般網址", () => {
  for (const url of [
    "/api/files/1/content",
    "#/folder/3",
    "?page=2",
    "",
    "https://example.com/",
    "http://192.168.1.10:3000/",
    "blob:http://localhost:3000/550e8400-e29b-41d4-a716-446655440000",
  ]) {
    assert.equal(isSafeUrl(url, BASE), true, JSON.stringify(url));
  }
});

test("擋下 javascript: 以及瀏覽器會還原的各種變形", () => {
  for (const url of [
    "javascript:alert(1)",
    "JavaScript:alert(1)",
    "JAVASCRIPT:alert(1)",
    " javascript:alert(1)", // 開頭空白
    "java\tscript:alert(1)", // 中間的 Tab：瀏覽器會刪掉
    "java\nscript:alert(1)", // 中間的換行：瀏覽器會刪掉
    "java\rscript:alert(1)", // 中間的歸位字元：瀏覽器會刪掉
    "\x01javascript:alert(1)", // 開頭的控制字元：瀏覽器會去掉
    "\x00javascript:alert(1)", // 開頭的 NUL
    "\x1fjavascript:alert(1)",
  ]) {
    assert.equal(isSafeUrl(url, BASE), false, JSON.stringify(url));
  }
});

test("擋下不在允許清單中的其他協定", () => {
  for (const url of [
    "data:text/html,<script>alert(1)</script>",
    "vbscript:msgbox(1)",
    "file:///etc/passwd",
    "mailto:someone@example.com",
  ]) {
    assert.equal(isSafeUrl(url, BASE), false, url);
  }
});

test("無法解析的網址視為不安全", () => {
  for (const url of ["http://[::1", "https://exa mple.com/"]) {
    assert.equal(isSafeUrl(url, BASE), false, url);
  }
});
