// 進入點：依登入狀態與網址決定要顯示哪個畫面。

import {
  api,
  ApiError,
  onPasswordChangeRequired,
  onUnauthorized,
} from "./api.js";
import { renderAdmin } from "./admin-view.js";
import { renderChangeCredentials, renderLogin } from "./auth-view.js";
import { renderBrowser } from "./browser-view.js";
import { h } from "./dom.js";
import { ADMIN_HREF, currentRoute, folderHref } from "./router.js";
import type { User } from "./types.js";

/** 取得 index.html 中一定存在的元素；不存在代表 HTML 與程式碼不同步，直接報錯 */
function requireElement(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (el === null) {
    throw new Error(`index.html 中找不到 #${id}`);
  }
  return el;
}

const app = requireElement("app");
const userSlot = requireElement("user");

/** 目前顯示的畫面。登入與強制修改的畫面不受網址影響 */
type Screen = "login" | "change" | "home" | "settings";
let screen: Screen = "login";

/** 目前登入的使用者；未登入時為 null */
let currentUser: User | null = null;

// ==================== 畫面 ====================

/**
 * 拿到使用者資料後，決定顯示哪個畫面。
 * 開啟頁面、登入成功、修改完成，全部經過這裡：判斷規則只寫在一個地方。
 */
function showScreen(user: User): void {
  currentUser = user;
  if (user.must_change_password) {
    screen = "change";
    showTopbar(user, { navigation: false });
    renderChangeCredentials(app, user, showScreen, { forced: true });
  } else {
    showHome(user);
  }
}

/** 依網址顯示「我的檔案」或「使用者管理」 */
function showHome(user: User): void {
  currentUser = user;
  screen = "home";
  showTopbar(user, { navigation: true });

  const route = currentRoute();
  if (route.view === "admin") {
    if (user.role === "admin") {
      // 降級自己之後重新讀取使用者，交給 showScreen 決定畫面
      renderAdmin(app, user, () => void refresh());
      return;
    }
    // 不是管理員：回到我的檔案。replaceState 不會觸發 hashchange，也不會留下歷史紀錄
    history.replaceState(null, "", folderHref(null));
  }
  renderBrowser(app, route.view === "files" ? route.folderId : null);
}

function showSettings(user: User): void {
  screen = "settings";
  showTopbar(user, { navigation: true });
  renderChangeCredentials(app, user, showScreen, {
    forced: false,
    onCancel: () => showHome(user),
  });
}

function showLoggedOut(notice?: string): void {
  screen = "login";
  currentUser = null;
  userSlot.replaceChildren();
  renderLogin(app, showScreen, notice);
}

/**
 * 頂部列：使用者名稱、我的檔案、使用者管理（只有管理員）、帳號設定、登出。
 * 必須修改密碼的狀態下只顯示登出。
 *
 * 「使用者管理」只對管理員顯示，但這只是使用體驗；真正的權限檢查在後端的 AdminUser。
 */
function showTopbar(user: User, { navigation }: { navigation: boolean }): void {
  // replaceChildren 是瀏覽器內建的方法，不接受 null（與 h() 不同），
  // 所以先組成陣列，只放入要顯示的項目
  const items: Node[] = [h("span", { class: "username" }, user.username)];

  if (navigation) {
    items.push(navButton("我的檔案", () => go(user, folderHref(null))));
    if (user.role === "admin") {
      items.push(navButton("使用者管理", () => go(user, ADMIN_HREF)));
    }
    items.push(navButton("帳號設定", () => showSettings(user)));
  }
  items.push(navButton("登出", () => void logout()));

  userSlot.replaceChildren(...items);
}

function navButton(text: string, onClick: () => void): HTMLButtonElement {
  return h("button", { type: "button", class: "link", onClick }, text);
}

/**
 * 前往某個網址。網址相同時 hashchange 不會觸發（例如在帳號設定頁點「我的檔案」，
 * 而網址本來就是 #/），所以直接重新顯示。
 */
function go(user: User, href: string): void {
  if (window.location.hash === href) {
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
    showError(error instanceof ApiError ? error.message : "登出失敗");
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
      showLoggedOut();
      return;
    }
    app.replaceChildren(
      h(
        "p",
        { class: "error", role: "alert" },
        error instanceof ApiError ? error.message : "發生未預期的錯誤",
      ),
    );
  }
}

// ==================== 全域事件 ====================

// 操作途中遇到 401（Cookie 過期、在其他分頁登出、被停用）：回到登入畫面並說明原因
onUnauthorized(() => showLoggedOut("登入已過期，請重新登入"));

// 任何 API 回「必須先修改密碼」：重新讀取使用者，切換到修改畫面
onPasswordChangeRequired(() => void refresh());

// 網址改變（點了資料夾、麵包屑、上一頁、頂部列的連結）：
// 登入後、且不在強制修改的畫面時，依網址顯示對應的頁面
window.addEventListener("hashchange", () => {
  if (currentUser !== null && (screen === "home" || screen === "settings")) {
    showHome(currentUser);
  }
});

// 檔案被拖放到上傳區域以外的地方時，瀏覽器預設會直接開啟那個檔案、離開 ManorCloud。
// 在整個視窗攔截預設行為；只有上傳區域（.dropzone）會實際處理放下的檔案。
window.addEventListener("dragover", (event) => event.preventDefault());
window.addEventListener("drop", (event) => event.preventDefault());

// void：明確表示不等待這個 Promise（refresh 內部已處理所有錯誤）
void refresh();
