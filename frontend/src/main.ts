// 進入點：依登入狀態與網址決定要顯示哪個畫面。

import { t } from "./i18n.js";
import { api, ApiError, onPasswordChangeRequired, onUnauthorized } from "./api.js";
import { renderChangeCredentials, renderLogin } from "./auth-view.js";
import { chooseFiles, chooseUploadFolder, leaveBrowser, mountPanels, newFolder, renderBrowser } from "./browser-view.js";
import { h } from "./dom.js";
import { renderPublicShare, renderShares, shareToken } from "./shares.js";
import { loadIconMap } from "./file-types.js";
import { closeMenus, dropdown, menuItem, menuSeparator } from "./menu.js";
import { adminHref, currentRoute, filesHref, MY_FILES, settingsHref } from "./router.js";
import { renderAdministration, renderSettings } from "./settings-view.js";
import { hideSidebar, renderSidebar } from "./sidebar.js";
import { takeSsoResult } from "./sso.js";
import { applyTheme, loadTheme } from "./theme.js";
import { showToast } from "./toast.js";
import { renderTwoFactorSetup } from "./two-factor-view.js";
import { leaveTrash, renderTrash } from "./trash-view.js";
import type { User } from "./types.js";

/** 取得 index.html 中一定存在的元素；不存在代表 HTML 與程式碼不同步，直接報錯 */
function requireElement(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (el === null) {
    throw new Error(`#${id} is missing from index.html`);
  }
  return el;
}

const app = requireElement("app");
const account = requireElement("account");
const sidebar = requireElement("sidebar");
const panels = requireElement("panels");
const brand = requireElement("brand");

// 最先套用主題：在畫出任何東西之前，避免先閃一下另一個主題的顏色
applyTheme(loadTheme());

mountPanels(panels);

/**
 * 從 SSO 回來時網址中的結果（失敗的原因，或剛完成連結）。只顯示一次：
 * 讀取時已經從網址列移除，顯示之後也清空
 */
let ssoResult = takeSsoResult();

/** 目前顯示的畫面。登入與強制修改的畫面不受網址影響；home 依網址決定顯示哪一頁 */
type Screen = "login" | "change" | "home";
let screen: Screen = "login";

/** 目前登入的使用者；未登入時為 null */
let currentUser: User | null = null;

const sidebarActions = { newFolder, upload: chooseFiles, uploadFolder: chooseUploadFolder };

// ==================== 畫面 ====================

/**
 * 拿到使用者資料後，決定顯示哪個畫面。
 * 開啟頁面、登入成功、修改完成，全部經過這裡：判斷規則只寫在一個地方。
 */
function showScreen(user: User): void {
  currentUser = user;
  panels.hidden = false;
  // 已登入：SSO 的結果用左下角的通知顯示（例如連結成功、連結失敗的原因）
  if (ssoResult.error !== null) {
    showToast(ssoResult.error);
  } else if (ssoResult.linked) {
    showToast(t("Your account is now linked"));
  }
  ssoResult = { error: null, linked: false };
  if (user.must_change_password) {
    screen = "change";
    leaveBrowser();
    hideSidebar(sidebar);
    showAccount(user, { navigation: false });
    renderChangeCredentials(app, user, showScreen, { forced: true });
  } else if (user.two_factor_setup_required) {
    // 管理員要求開啟 2FA，而這個帳號還沒有設定：完成之前不能使用其他功能（後端也會擋下）
    screen = "change";
    leaveBrowser();
    leaveTrash();
    hideSidebar(sidebar);
    showAccount(user, { navigation: false });
    const steps = h("div", {});
    app.replaceChildren(
      h(
        "div",
        { class: "auth-form" },
        h("h1", {}, t("Set up two-factor authentication")),
        h(
          "p",
          { class: "notice", role: "status" },
          t("Your administrator requires two-factor authentication. Set it up to continue."),
        ),
        steps,
      ),
    );
    renderTwoFactorSetup(steps, { onDone: () => void refresh() });
  } else {
    showHome(user);
  }
}

/** 依網址顯示 My files、Trash、Settings 或 Administration */
function showHome(user: User): void {
  currentUser = user;
  screen = "home";
  closeMenus();
  showAccount(user, { navigation: true });
  leaveTrash();
  const isAdmin = user.role === "admin";
  const admin = { admin: isAdmin, userId: user.id };

  const route = currentRoute();
  // 資料目錄的最上層只有管理員能看：一般使用者回到自己的檔案。
  // 其他硬碟可能掛載給了他，由後端決定能不能打開（不能時畫面顯示錯誤與返回的連結）。
  // 這只是使用體驗；真正的權限檢查在後端
  if (
    !isAdmin &&
    (route.view === "files" || route.view === "trash") &&
    route.location.kind === "disk" &&
    route.location.disk === null
  ) {
    history.replaceState(null, "", filesHref(MY_FILES));
    showHome(user);
    return;
  }

  switch (route.view) {
    case "trash":
      leaveBrowser();
      renderSidebar(sidebar, sidebarActions, "trash", { ...admin, location: route.location });
      renderTrash(app, { ...admin, location: route.location });
      return;

    case "shared":
      leaveBrowser();
      renderSidebar(sidebar, sidebarActions, "shared", admin);
      renderShares(app, isAdmin);
      return;

    case "settings":
      leaveBrowser();
      renderSidebar(sidebar, sidebarActions, null, admin);
      // 修改了帳號名稱或密碼：交給 showScreen 依新的使用者資料重新顯示
      renderSettings(app, user, route.section, showScreen);
      return;

    case "admin":
      if (user.role === "admin") {
        leaveBrowser();
        renderSidebar(sidebar, sidebarActions, null, admin);
        // 降級自己之後重新讀取使用者，交給 showScreen 決定畫面
        renderAdministration(app, user, route.section, () => void refresh());
        return;
      }
      // 不是管理員：回到 My files。replaceState 不會觸發 hashchange，也不會留下歷史紀錄。
      // 這只是使用體驗；真正的權限檢查在後端的 AdminUser
      history.replaceState(null, "", filesHref(MY_FILES));
      renderSidebar(sidebar, sidebarActions, "files", admin);
      renderBrowser(app, { ...admin, location: MY_FILES, path: [] });
      return;

    case "files":
      renderSidebar(sidebar, sidebarActions, "files", { ...admin, location: route.location });
      renderBrowser(app, { ...admin, location: route.location, path: route.path });
      return;
  }
}

function showLoggedOut(notice?: string): void {
  screen = "login";
  currentUser = null;
  leaveBrowser();
  leaveTrash();
  closeMenus();
  account.replaceChildren();
  hideSidebar(sidebar);
  panels.hidden = true;
  renderLogin(app, showScreen, notice);
}

/**
 * 右上角的頭像與帳號選單：名稱與身分、Settings、Administration（只有管理員）、Log out。
 * 必須修改密碼的狀態下只有 Log out。
 *
 * 「Administration」只對管理員顯示，但這只是使用體驗；真正的權限檢查在後端的 AdminUser。
 */
function showAccount(user: User, { navigation }: { navigation: boolean }): void {
  const avatar = h(
    "button",
    {
      type: "button",
      class: "avatar",
      "aria-label": t("Account menu for {name}", { name: user.username }),
      title: user.username,
    },
    // 帳號名稱只能是小寫英文、數字、底線與連字號，第一個字一定能顯示
    user.username.charAt(0).toUpperCase(),
  );

  const items: Node[] = [
    h(
      "div",
      { class: "menu-header" },
      h("strong", {}, user.username),
      h("span", { class: "muted" }, user.role === "admin" ? t("Administrator") : t("Member")),
    ),
  ];
  if (navigation) {
    items.push(
      menuItem({ label: t("Settings"), icon: "user", onSelect: () => go(user, settingsHref()) }),
    );
    if (user.role === "admin") {
      items.push(
        menuItem({ label: t("Administration"), icon: "users", onSelect: () => go(user, adminHref()) }),
      );
    }
    items.push(menuSeparator());
  }
  items.push(menuItem({ label: t("Log out"), icon: "logout", onSelect: () => void logout() }));

  const menu = h(
    "div",
    { class: "menu dropdown-menu account-menu", role: "menu", "aria-label": t("Account") },
    items,
  );
  dropdown(avatar, menu);
  account.replaceChildren(avatar, menu);
}

/**
 * 前往某個網址。網址相同時 hashchange 不會觸發（例如在帳號設定頁點 My files，
 * 而網址本來就是 #/），所以直接重新顯示。
 */
function go(user: User, href: string): void {
  if (window.location.hash === href || (href === filesHref(MY_FILES) && window.location.hash === "")) {
    showHome(user);
  } else {
    window.location.hash = href;
  }
}

/** 顯示一則全域錯誤。同時只保留一則，連續失敗不會疊出好幾條相同的訊息 */
function showError(message: string): void {
  app.querySelector(".global-error")?.remove();
  app.prepend(h("p", { class: "error global-error", role: "alert" }, message));
}

// ==================== 動作 ====================

async function logout(): Promise<void> {
  try {
    await api.post("/api/logout");
  } catch (error) {
    // 登出失敗（例如連不到伺服器）時，Cookie 仍然有效，不能假裝已經登出
    showError(error instanceof ApiError ? error.message : t("Couldn't log out"));
    return;
  }
  showLoggedOut();
}

/**
 * 重新讀取目前的使用者並顯示對應的畫面。
 * 開啟頁面、其他 API 回「必須先修改密碼」、或修改了自己的角色時呼叫。
 * /api/me 允許「必須修改密碼」的狀態，所以這裡不會再觸發 password_change_required，不會無限循環。
 */
async function refresh(): Promise<void> {
  try {
    // 首次開啟頁面時的 401 只代表「還沒登入」，不是「已過期」，所以不通知 onUnauthorized
    const user = await api.get<User>("/api/me", { notifyUnauthorized: false });
    showScreen(user);
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) {
      // 沒有登入：SSO 失敗的原因顯示在登入畫面上
      const notice = ssoResult.error ?? undefined;
      ssoResult = { error: null, linked: false };
      showLoggedOut(notice);
      return;
    }
    app.replaceChildren(
      h(
        "p",
        { class: "error", role: "alert" },
        error instanceof ApiError ? error.message : t("Something went wrong"),
      ),
    );
  }
}

// ==================== 全域事件 ====================

// 操作途中遇到 401（Cookie 過期、在其他分頁登出、被停用）：回到登入畫面並說明原因
onUnauthorized(() => showLoggedOut(t("Your session has expired. Log in again.")));

// 任何 API 回「必須先修改密碼」：重新讀取使用者，切換到修改畫面
onPasswordChangeRequired(() => void refresh());

// 網址改變（點了資料夾、路徑、上一頁、側欄的連結）：
// 登入後、且不在強制修改的畫面時，依網址顯示對應的頁面
window.addEventListener("hashchange", () => {
  if (currentUser !== null && screen === "home") {
    showHome(currentUser);
  }
});

// 點 ManorCloud 回到 My files；在帳號設定頁時網址可能沒有改變，所以自己處理
brand.addEventListener("click", (event) => {
  if (currentUser !== null && screen !== "change") {
    event.preventDefault();
    go(currentUser, filesHref(MY_FILES));
  }
});

// 側欄的連結（My files、Trash）：前往連結本身的網址。
// 自己處理而不交給瀏覽器，是為了網址沒有改變時也能重新顯示（例如從帳號設定頁回到 My files）
sidebar.addEventListener("click", (event) => {
  const target = event.target;
  if (currentUser === null || !(target instanceof Element)) {
    return;
  }
  const link = target.closest<HTMLAnchorElement>("a.nav-link");
  if (link === null) {
    return;
  }
  event.preventDefault();
  go(currentUser, link.getAttribute("href") ?? filesHref(MY_FILES));
});

// 檔案被拖放到上傳區域以外的地方時，瀏覽器預設會直接開啟那個檔案、離開 ManorCloud。
// 在整個視窗攔截預設行為；只有檔案頁面的內容區會實際處理放下的檔案。
window.addEventListener("dragover", (event) => event.preventDefault());
window.addEventListener("drop", (event) => event.preventDefault());

// 分享連結的網頁（/s/{代碼}）：給沒有帳號的人看，不需要登入，也不顯示側欄與帳號選單
const token = shareToken();
if (token !== null) {
  brand.setAttribute("href", "/");
  panels.hidden = true;
  void loadIconMap().then(() => renderPublicShare(app, token));
} else {
  // void：明確表示不等待這個 Promise（refresh 內部已處理所有錯誤）
  // 圖示的對應表先載入，第一次顯示列表時就有正確的圖示
  void loadIconMap().then(() => refresh());
}
