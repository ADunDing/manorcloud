// 顯示用的格式化函式

import { t } from "./i18n.js";

const UNITS = ["B", "KB", "MB", "GB", "TB"];

/** 位元組數轉成人看得懂的大小，以 1024 為單位。null（資料夾）顯示為破折號 */
export function formatSize(bytes: number | null): string {
  if (bytes === null) {
    return "—";
  }
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  // 位元組不顯示小數；其他單位保留一位
  const text = unit === 0 ? String(Math.round(value)) : value.toFixed(1);
  return `${text} ${UNITS[unit] ?? ""}`;
}

/**
 * 日期與時間：2026-11-10 14:22（使用者電腦的時區）。
 * 不依賴語系，每個人看到的格式都一樣；之後做多國語言時再依語言調整。
 */
function formatDate(date: Date): string {
  const pad = (n: number): string => String(n).padStart(2, "0");
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}`
  );
}

/** Unix 秒數轉成本地時間 */
export function formatTime(seconds: number): string {
  return formatDate(new Date(seconds * 1000));
}

/**
 * SQLite CURRENT_TIMESTAMP 的文字（UTC，例如 "2026-11-01 07:23:59"）轉成本地時間。
 * 無法解析時原樣顯示，不會因為格式不同而顯示成 Invalid Date。
 */
export function formatUtcText(text: string): string {
  const date = new Date(`${text.replace(" ", "T")}Z`);
  return Number.isNaN(date.getTime()) ? text : formatDate(date);
}

/** 秒數轉成「1 min 30 s」這樣的長度，用於剩餘時間 */
export function formatDuration(seconds: number): string {
  const total = Math.max(1, Math.round(seconds));
  if (total < 60) {
    return t("{s} s", { s: total });
  }
  const minutes = Math.floor(total / 60);
  if (minutes < 60) {
    return t("{m} min {s} s", { m: minutes, s: total % 60 });
  }
  return t("{h} h {m} min", { h: Math.floor(minutes / 60), m: minutes % 60 });
}
