// 從 pdfjs-dist 套件（devDependencies）複製 PDF 預覽需要的檔案到 public/vendor/pdfjs/：
//   pdf.min.mjs、pdf.worker.min.mjs   顯示 PDF 的程式（預覽 PDF 時才載入）
//   cmaps/、standard_fonts/、iccs/   中日韓文字的對應表、沒有內嵌的標準字型、色彩設定
//   wasm/*_nowasm_fallback.js       特殊圖片格式的解碼器（網站的安全政策不允許 WebAssembly，用 JS 版本）
//   LICENSE*                        授權（Apache-2.0 等）
// npm run build 時執行；產生的檔案不放進 git。

import { cpSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);
const pkg = dirname(require.resolve("pdfjs-dist/package.json"));
const out = new URL("../public/vendor/pdfjs/", import.meta.url).pathname;

rmSync(out, { recursive: true, force: true });
mkdirSync(join(out, "wasm"), { recursive: true });
for (const file of ["build/pdf.min.mjs", "build/pdf.worker.min.mjs", "LICENSE"]) {
  cpSync(join(pkg, file), join(out, file.replace("build/", "")));
}
for (const dir of ["cmaps", "standard_fonts", "iccs"]) {
  cpSync(join(pkg, dir), join(out, dir), { recursive: true });
}
for (const file of readdirSync(join(pkg, "wasm"))) {
  if (file.endsWith("_nowasm_fallback.js") || file.startsWith("LICENSE")) {
    cpSync(join(pkg, "wasm", file), join(out, "wasm", file));
  }
}
console.log("pdfjs: copied");
