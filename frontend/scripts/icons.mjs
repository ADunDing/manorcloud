// 從 material-icon-theme 套件（devDependencies）產生檔案列表的圖示：
//   public/icons/*.svg      用得到的圖示
//   public/icons/map.json   副檔名、檔名、資料夾名稱 → 圖示名稱（對應方式與 VS Code 相同）
//   public/icons/LICENSE    圖示的授權（MIT）
// npm run build 時執行；產生的檔案不放進 git。

import { copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);
const pkg = dirname(require.resolve("material-icon-theme/package.json"));
const manifest = JSON.parse(readFileSync(join(pkg, "dist/material-icons.json"), "utf8"));
const out = new URL("../public/icons/", import.meta.url).pathname;

/** 套件的對應不適合雲端硬碟的地方（它是給程式設計師用的） */
const OVERRIDES = {
  ext: {
    ogg: "audio",
    img: "disc",
    vhd: "disc",
    vhdx: "disc",
    key: "powerpoint",
    m2ts: "video",
    "3gp": "video",
    pages: "word",
    numbers: "table",
  },
};

/** 名稱都轉成小寫：比對時不分大小寫 */
function lower(map = {}) {
  return Object.fromEntries(Object.entries(map).map(([k, v]) => [k.toLowerCase(), v]));
}

const map = {
  file: manifest.file,
  folder: manifest.folder,
  ext: { ...lower(manifest.fileExtensions), ...OVERRIDES.ext },
  names: lower(manifest.fileNames),
  folders: lower(manifest.folderNames),
  light: {
    ext: lower(manifest.light?.fileExtensions),
    names: lower(manifest.light?.fileNames),
    folders: lower(manifest.light?.folderNames),
  },
};

// 只複製對應表中用到的圖示
const used = new Set([map.file, map.folder]);
for (const table of [map.ext, map.names, map.folders, map.light.ext, map.light.names, map.light.folders]) {
  for (const icon of Object.values(table)) {
    used.add(icon);
  }
}

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
let missing = 0;
for (const icon of used) {
  const definition = manifest.iconDefinitions[icon];
  if (definition === undefined) {
    missing += 1;
    continue;
  }
  copyFileSync(join(pkg, "dist", definition.iconPath), join(out, `${icon}.svg`));
}
writeFileSync(join(out, "map.json"), JSON.stringify(map));
copyFileSync(join(pkg, "LICENSE"), join(out, "LICENSE"));
console.log(`icons: ${used.size - missing} icons${missing > 0 ? `, ${missing} without a file` : ""}`);
