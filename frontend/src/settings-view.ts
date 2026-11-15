// Settings（每個人）與 Administration（管理員）兩個頁面：左邊是區塊清單，右邊是內容。
//
// 新的區塊（例如 SSO 的 Sign-in、儲存位置的 Storage）：在 router.ts 的清單加上名稱，
// 再在這裡的 SECTIONS 與 renderSection 各加一項。

import { renderAdmin } from "./admin-view.js";
import { renderChangeCredentials } from "./auth-view.js";
import { h } from "./dom.js";
import { loadView, saveView, type ViewMode } from "./file-list.js";
import { icon, type IconName } from "./icons.js";
import {
  adminHref,
  settingsHref,
  type AdminSection,
  type SettingsSection,
} from "./router.js";
import { applyTheme, loadTheme, saveTheme, type Theme } from "./theme.js";
import { showToast } from "./toast.js";
import type { User } from "./types.js";

interface NavItem {
  readonly href: string;
  readonly label: string;
  readonly icon: IconName;
  readonly active: boolean;
}

// ==================== Settings ====================

const SETTINGS: readonly { section: SettingsSection; label: string; icon: IconName }[] = [
  { section: "account", label: "Account", icon: "user" },
  { section: "appearance", label: "Appearance", icon: "grid" },
];

/**
 * @param onAccountChanged 帳號名稱或密碼修改成功後呼叫，傳入更新後的使用者
 */
export function renderSettings(
  container: HTMLElement,
  user: User,
  section: SettingsSection,
  onAccountChanged: (user: User) => void,
): void {
  const panel = layout(
    container,
    "Settings",
    SETTINGS.map((item) => ({
      href: settingsHref(item.section),
      label: item.label,
      icon: item.icon,
      active: item.section === section,
    })),
  );

  switch (section) {
    case "account":
      renderChangeCredentials(
        panel,
        user,
        (updated) => {
          showToast("Your account was updated");
          onAccountChanged(updated);
        },
        { forced: false, embedded: true },
      );
      break;
    case "appearance":
      renderAppearance(panel);
      break;
  }
}

function renderAppearance(panel: HTMLElement): void {
  const themes: [Theme, string, string][] = [
    ["system", "Match my device", "Light or dark, following your computer's setting"],
    ["light", "Light", "Always light"],
    ["dark", "Dark", "Always dark"],
  ];
  const current = loadTheme();
  const themeGroup = group(
    "Theme",
    themes.map(([value, label, hint]) =>
      choice("theme", value === current, label, hint, () => {
        saveTheme(value);
        applyTheme(value);
      }),
    ),
  );

  const views: [ViewMode, string, string][] = [
    ["list", "List", "Names, sizes and dates in rows"],
    ["grid", "Grid", "Large icons in a grid"],
  ];
  const view = loadView();
  const viewGroup = group(
    "File view",
    views.map(([value, label, hint]) =>
      choice("view", value === view, label, hint, () => saveView(value)),
    ),
    "You can also switch with the buttons at the top right of My files.",
  );

  panel.replaceChildren(h("h2", { class: "section-title" }, "Appearance"), themeGroup, viewGroup);
}

// ==================== Administration ====================

const ADMIN: readonly { section: AdminSection; label: string; icon: IconName }[] = [
  { section: "users", label: "Users", icon: "users" },
];

/**
 * @param onSelfChanged 修改了自己的角色之後呼叫（例如降級為成員，已經不能再看這一頁）
 */
export function renderAdministration(
  container: HTMLElement,
  me: User,
  section: AdminSection,
  onSelfChanged: () => void,
): void {
  const panel = layout(
    container,
    "Administration",
    ADMIN.map((item) => ({
      href: adminHref(item.section),
      label: item.label,
      icon: item.icon,
      active: item.section === section,
    })),
  );

  switch (section) {
    case "users":
      renderAdmin(panel, me, onSelfChanged);
      break;
  }
}

// ==================== 共用的版面 ====================

/** 標題、左邊的區塊清單與右邊的內容區；回傳內容區，由各區塊填入 */
function layout(container: HTMLElement, title: string, items: readonly NavItem[]): HTMLElement {
  const panel = h("section", { class: "settings-panel" });
  container.replaceChildren(
    h("h1", { class: "page-title" }, title),
    h(
      "div",
      { class: "settings-page" },
      h(
        "nav",
        { class: "settings-nav", "aria-label": `${title} sections` },
        h(
          "ul",
          {},
          items.map((item) =>
            h(
              "li",
              {},
              h(
                "a",
                {
                  href: item.href,
                  class: "settings-link",
                  "aria-current": item.active ? "page" : null,
                },
                icon(item.icon, 18),
                item.label,
              ),
            ),
          ),
        ),
      ),
      panel,
    ),
  );
  return panel;
}

function group(legend: string, choices: HTMLElement[], note?: string): HTMLElement {
  return h(
    "fieldset",
    { class: "settings-group" },
    h("legend", {}, legend),
    h("div", { class: "choices" }, choices),
    note === undefined ? null : h("p", { class: "hint" }, note),
  );
}

/** 一個選項。選了立刻生效，不需要按儲存 */
function choice(
  name: string,
  checked: boolean,
  label: string,
  hint: string,
  onSelect: () => void,
): HTMLElement {
  const input = h("input", { type: "radio", name, checked });
  input.addEventListener("change", () => {
    if (input.checked) {
      onSelect();
    }
  });
  return h(
    "label",
    { class: "choice" },
    input,
    h("span", { class: "choice-text" }, h("strong", {}, label), h("span", { class: "hint" }, hint)),
  );
}
