// 翻譯表的檢查：介面上每個 t("...")、tNodes("...") 都有繁體中文翻譯，翻譯表裡沒有用不到的，
// 而且翻譯保留了原文所有的 {名稱}。

import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { ZH_TW_UI } from "../public/js/locales/zh-tw.js";

const SRC = new URL("../src/", import.meta.url);

/** 原始碼中所有要翻譯的英文 */
function keysInSource() {
  const keys = new Set();
  for (const file of readdirSync(SRC)) {
    if (!file.endsWith(".ts") || file === "i18n.ts") {
      continue;
    }
    const text = readFileSync(new URL(file, SRC), "utf8");
    for (const match of text.matchAll(/\b(?:t|tNodes)\(\s*("(?:[^"\\]|\\.)*")/g)) {
      keys.add(JSON.parse(match[1]));
    }
  }
  return keys;
}

const placeholders = (text) => [...text.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();

test("every text on screen has a Traditional Chinese translation", () => {
  const missing = [...keysInSource()].filter((key) => !Object.hasOwn(ZH_TW_UI, key));
  assert.deepEqual(missing, []);
});

test("the translation table has no unused entries", () => {
  const keys = keysInSource();
  const unused = Object.keys(ZH_TW_UI).filter((key) => !keys.has(key));
  assert.deepEqual(unused, []);
});

test("translations keep every {placeholder}", () => {
  for (const [key, value] of Object.entries(ZH_TW_UI)) {
    assert.deepEqual([...new Set(placeholders(value))], [...new Set(placeholders(key))], key);
  }
});
