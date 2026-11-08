// 顯示用的格式化函式

const UNITS = ["B", "KB", "MB", "GB", "TB"];

const DATE_FORMAT: Intl.DateTimeFormatOptions = {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
};

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
  const text = unit === 0 ? String(value) : value.toFixed(1);
  return `${text} ${UNITS[unit] ?? ""}`;
}

/** Unix 秒數轉成本地時間 */
export function formatTime(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString("zh-TW", DATE_FORMAT);
}

/**
 * SQLite CURRENT_TIMESTAMP 的文字（UTC，例如 "2026-11-01 07:23:59"）轉成本地時間。
 * 無法解析時原樣顯示，不會因為格式不同而顯示成 Invalid Date。
 */
export function formatUtcText(text: string): string {
  const date = new Date(`${text.replace(" ", "T")}Z`);
  return Number.isNaN(date.getTime())
    ? text
    : date.toLocaleString("zh-TW", DATE_FORMAT);
}
