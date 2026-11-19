// 檔案列表中的圖示：Material Icon Theme（與 VS Code 的同名套件相同）。
// 圖示與對應表由 scripts/icons.mjs 在 npm run build 時產生在 public/icons/。
// 只是外觀：不影響檔案的處理方式。對應表載入失敗時，全部顯示一般的檔案與資料夾圖示。

/** public/icons/map.json，名稱一律是小寫 */
interface IconMap {
  readonly file: string;
  readonly folder: string;
  readonly ext: Readonly<Record<string, string>>;
  readonly names: Readonly<Record<string, string>>;
  readonly folders: Readonly<Record<string, string>>;
  /** 淺色主題中改用的圖示 */
  readonly light: {
    readonly ext: Readonly<Record<string, string>>;
    readonly names: Readonly<Record<string, string>>;
    readonly folders: Readonly<Record<string, string>>;
  };
}

const EMPTY: IconMap = {
  file: "file",
  folder: "folder",
  ext: {},
  names: {},
  folders: {},
  light: { ext: {}, names: {}, folders: {} },
};

let icons: IconMap = EMPTY;

/** 開啟網頁時載入一次對應表 */
export async function loadIconMap(): Promise<void> {
  try {
    const response = await fetch("/icons/map.json");
    if (response.ok) {
      icons = (await response.json()) as IconMap;
    }
  } catch {
    // 只是圖示：用一般的圖示
  }
}

/** 用 hasOwn：檔名是 constructor 之類的字時，不會拿到物件本身的屬性 */
function lookup(table: Readonly<Record<string, string>>, key: string): string | undefined {
  return Object.hasOwn(table, key) ? table[key] : undefined;
}

/** 目前是淺色主題（設定成淺色，或跟隨系統而系統是淺色） */
function isLight(): boolean {
  const theme = document.documentElement.dataset["theme"];
  return theme === "light" || (theme === undefined && !window.matchMedia("(prefers-color-scheme: dark)").matches);
}

/** 先查淺色主題的版本，再查一般的 */
function find(
  kind: "ext" | "names" | "folders",
  key: string,
): string | undefined {
  return (isLight() ? lookup(icons.light[kind], key) : undefined) ?? lookup(icons[kind], key);
}

/** 檔案的圖示名稱：先比對整個檔名，再從最長的副檔名開始比對（例如 a.test.ts → test.ts → ts） */
export function fileIconName(name: string): string {
  const lower = name.toLowerCase();
  const byName = find("names", lower);
  if (byName !== undefined) {
    return byName;
  }
  // .env 這類以點開頭的名稱，點後面的整段也算副檔名
  for (let dot = lower.indexOf("."); dot >= 0; dot = lower.indexOf(".", dot + 1)) {
    const byExt = find("ext", lower.slice(dot + 1));
    if (byExt !== undefined) {
      return byExt;
    }
  }
  return icons.file;
}

/** 資料夾的圖示名稱（例如 Photos、Music、Downloads 有專屬的圖示） */
export function folderIconName(name: string): string {
  return find("folders", name.toLowerCase()) ?? icons.folder;
}

/** 圖示的網址 */
export function iconUrl(icon: string): string {
  return `/icons/${icon}.svg`;
}
