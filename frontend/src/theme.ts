// 外觀主題：跟隨系統、淺色、深色。
//
// 選擇記在瀏覽器中（換一台電腦要重新設定）。
// 套用方式：在 <html> 加上 data-theme="light" 或 "dark"；跟隨系統時移除這個屬性，
// 由 CSS 的 prefers-color-scheme 決定（見 app.css 開頭的顏色變數）。

export type Theme = "system" | "light" | "dark";

const STORAGE_KEY = "manorcloud.theme";

export function loadTheme(): Theme {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return value === "light" || value === "dark" ? value : "system";
  } catch {
    return "system";
  }
}

export function saveTheme(theme: Theme): void {
  try {
    if (theme === "system") {
      localStorage.removeItem(STORAGE_KEY);
    } else {
      localStorage.setItem(STORAGE_KEY, theme);
    }
  } catch {
    // 無法儲存只代表下次回到跟隨系統
  }
}

export function applyTheme(theme: Theme): void {
  if (theme === "system") {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = theme;
  }
}
