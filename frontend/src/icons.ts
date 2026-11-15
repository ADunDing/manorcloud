// 介面中的線條圖示。
//
// h() 只能建立 HTML 元素；SVG 必須用 createElementNS 建立在 SVG 的命名空間中。
// 圖示的形狀寫死在這裡，不經過 innerHTML，與 dom.ts「不把字串當成 HTML 解析」的原則一致。

const SVG_NS = "http://www.w3.org/2000/svg";

export type IconName =
  | "folder"
  | "file"
  | "newFolder"
  | "upload"
  | "download"
  | "move"
  | "copy"
  | "trash"
  | "rename"
  | "list"
  | "grid"
  | "plus"
  | "close"
  | "chevronUp"
  | "user"
  | "users"
  | "logout"
  | "disk"
  | "sort"
  | "arrowUp"
  | "arrowDown"
  | "check";

/** 每個圖示的路徑，座標系為 24 × 24 */
const PATHS: Readonly<Record<IconName, readonly string[]>> = {
  folder: [
    "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
  ],
  file: ["M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z", "M14 3v5h5"],
  newFolder: [
    "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
    "M12 10v6M9 13h6",
  ],
  upload: ["M12 15V4", "M7 9l5-5 5 5", "M4 15v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3"],
  download: ["M12 4v11", "M7 10l5 5 5-5", "M4 19h16"],
  move: [
    "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
    "M10 13h6M13 10l3 3-3 3",
  ],
  copy: [
    "M11 9h7a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2h-7a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2z",
    "M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1",
  ],
  trash: [
    "M4 7h16",
    "M10 11v6M14 11v6",
    "M6 7l1 12a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-12",
    "M9 7V4h6v3",
  ],
  rename: ["M4 20h4l10.5-10.5a2.1 2.1 0 0 0-3-3L5 17v3z", "M13.5 7.5l3 3"],
  list: ["M8 6h13M8 12h13M8 18h13", "M3.5 6h.01M3.5 12h.01M3.5 18h.01"],
  grid: [
    "M5.5 4h4a1.5 1.5 0 0 1 1.5 1.5v4a1.5 1.5 0 0 1-1.5 1.5h-4A1.5 1.5 0 0 1 4 9.5v-4A1.5 1.5 0 0 1 5.5 4z",
    "M14.5 4h4a1.5 1.5 0 0 1 1.5 1.5v4a1.5 1.5 0 0 1-1.5 1.5h-4A1.5 1.5 0 0 1 13 9.5v-4A1.5 1.5 0 0 1 14.5 4z",
    "M5.5 13h4a1.5 1.5 0 0 1 1.5 1.5v4a1.5 1.5 0 0 1-1.5 1.5h-4A1.5 1.5 0 0 1 4 18.5v-4A1.5 1.5 0 0 1 5.5 13z",
    "M14.5 13h4a1.5 1.5 0 0 1 1.5 1.5v4a1.5 1.5 0 0 1-1.5 1.5h-4a1.5 1.5 0 0 1-1.5-1.5v-4a1.5 1.5 0 0 1 1.5-1.5z",
  ],
  plus: ["M12 5v14M5 12h14"],
  close: ["M6 6l12 12M18 6L6 18"],
  chevronUp: ["M6 15l6-6 6 6"],
  user: ["M12 4a4 4 0 1 0 0 8a4 4 0 1 0 0-8z", "M4 21a8 8 0 0 1 16 0"],
  users: [
    "M9 4.5a3.5 3.5 0 1 0 0 7a3.5 3.5 0 1 0 0-7z",
    "M2.5 20a6.5 6.5 0 0 1 13 0",
    "M16 4.5a3.5 3.5 0 0 1 0 7",
    "M18.5 14a6.5 6.5 0 0 1 3 6",
  ],
  logout: ["M15 4h3a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-3", "M10 17l5-5-5-5", "M15 12H4"],
  sort: ["M4 7h16", "M7 12h10", "M10 17h4"],
  arrowUp: ["M12 19V5", "M6 11l6-6 6 6"],
  arrowDown: ["M12 5v14", "M6 13l6 6 6-6"],
  check: ["M5 12l5 5 9-10"],
  disk: [
    "M5 4h14a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z",
    "M5 13h14a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2z",
    "M7 7.5h.01M7 16.5h.01",
  ],
};

/**
 * 建立一個圖示。顏色跟隨文字顏色（currentColor）。
 * 圖示只是裝飾：對螢幕閱讀器隱藏，按鈕的名稱由 aria-label 或文字提供。
 */
export function icon(name: IconName, size = 18): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", size >= 32 ? "1.4" : "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");

  for (const d of PATHS[name]) {
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", d);
    svg.append(path);
  }
  return svg;
}
