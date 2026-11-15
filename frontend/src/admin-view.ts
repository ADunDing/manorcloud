// 使用者管理：建立帳號、指派管理員、停用、重設密碼。
//
// 臨時密碼只存在顯示它的面板元素裡，不放進任何變數或狀態；
// 關閉面板時先清空值再移除元素，不會殘留在 DOM 中。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { confirmDialog } from "./dialogs.js";
import { h } from "./dom.js";
import { formatUtcText } from "./format.js";
import type { CreatedUser, ManagedUser, ResetPasswordResponse, User } from "./types.js";

// 與後端 src/web/auth.rs 的帳號規則一致（同 auth-view.ts）。後端才是唯一的權威
const USERNAME_MIN = 3;
const USERNAME_MAX = 32;
const USERNAME_PATTERN = "[a-z0-9_\\-]+";
const USERNAME_HINT = `${USERNAME_MIN}–${USERNAME_MAX} characters: lowercase letters, digits, underscores and hyphens`;

/**
 * 在 container 中顯示使用者管理頁。
 *
 * @param me            目前登入的管理員
 * @param onSelfChanged 修改了自己的角色之後呼叫（例如降級為成員，已經不能再看這一頁）
 */
export function renderAdmin(container: HTMLElement, me: User, onSelfChanged: () => void): void {
  new AdminView(container, me, onSelfChanged).mount();
}

class AdminView {
  /** 臨時密碼面板的位置。與列表分開：重新載入列表時，面板不會被清掉 */
  private readonly noticeSlot = h("div", {});
  private readonly errorBox = h("p", { class: "error", role: "alert" });
  private readonly tableSlot = h("div", {});
  /** 操作進行中，避免重複送出 */
  private busy = false;

  constructor(
    private readonly container: HTMLElement,
    private readonly me: User,
    private readonly onSelfChanged: () => void,
  ) {}

  mount(): void {
    this.container.replaceChildren(
      h("h1", { class: "page-title" }, "Users"),
      this.noticeSlot,
      this.createForm(),
      this.errorBox,
      this.tableSlot,
    );
    void this.loadTable();
  }

  // ---------- 列表 ----------

  /** 只重新載入列表這一塊，臨時密碼面板保留 */
  private async loadTable(): Promise<void> {
    this.tableSlot.replaceChildren(h("p", { class: "muted" }, "Loading…"));
    try {
      const users = await api.get<ManagedUser[]>("/api/admin/users");
      this.tableSlot.replaceChildren(this.table(users));
    } catch (error) {
      if (!isHandledGlobally(error)) {
        this.tableSlot.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
      }
    }
  }

  private table(users: readonly ManagedUser[]): HTMLElement {
    return h(
      "table",
      { class: "users" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", { scope: "col" }, "Username"),
          h("th", { scope: "col" }, "Role"),
          h("th", { scope: "col" }, "Status"),
          h("th", { scope: "col" }, "Created"),
          h("th", { scope: "col" }, h("span", { class: "visually-hidden" }, "Actions")),
        ),
      ),
      h(
        "tbody",
        {},
        users.map((user) => this.row(user)),
      ),
    );
  }

  private row(user: ManagedUser): HTMLElement {
    const isSelf = user.id === this.me.id;
    const status = user.disabled
      ? "Disabled"
      : user.must_change_password
        ? "Waiting for first login"
        : "Active";

    return h(
      "tr",
      { class: user.disabled ? "disabled-row" : null },
      h("td", {}, user.username, isSelf ? h("span", { class: "muted" }, " (you)") : null),
      h(
        "td",
        {},
        h("span", { class: `badge ${user.role}` }, user.role === "admin" ? "Administrator" : "Member"),
      ),
      h("td", {}, status),
      h("td", {}, formatUtcText(user.created_at)),
      h(
        "td",
        { class: "actions-cell" },
        h(
          "button",
          { type: "button", class: "link", onClick: () => void this.toggleRole(user, isSelf) },
          user.role === "admin" ? "Remove admin" : "Make admin",
        ),
        // 後端不允許停用自己、重設自己的密碼：自己那一列不顯示這兩個按鈕
        isSelf
          ? null
          : h(
              "button",
              { type: "button", class: "link", onClick: () => void this.toggleDisabled(user) },
              user.disabled ? "Enable" : "Disable",
            ),
        isSelf
          ? null
          : h(
              "button",
              { type: "button", class: "link", onClick: () => void this.resetPassword(user) },
              "Reset password",
            ),
      ),
    );
  }

  // ---------- 建立 ----------

  private createForm(): HTMLElement {
    const input = h("input", {
      name: "username",
      type: "text",
      required: true,
      // 這是別人的帳號名稱，不要讓密碼管理器把它當成自己的帳號填入
      autocomplete: "off",
      autocapitalize: "none",
      spellcheck: "false",
      minlength: USERNAME_MIN,
      maxlength: USERNAME_MAX,
      pattern: USERNAME_PATTERN,
      title: USERNAME_HINT,
    });

    // method="post"：萬一 JS 沒有載入，表單內容也不會出現在網址裡
    const form = h(
      "form",
      { method: "post", class: "admin-create" },
      h("label", {}, "New username", input),
      h("button", { type: "submit" }, "Create user"),
    );

    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const username = input.value;
      this.perform(async () => {
        const created = await api.post<CreatedUser>("/api/admin/users", { username });
        input.value = "";
        this.showPassword(created.user.username, created.temporary_password, "created");
      });
    });

    return form;
  }

  // ---------- 修改 ----------

  private async toggleRole(user: ManagedUser, isSelf: boolean): Promise<void> {
    const promote = user.role !== "admin";
    const confirmed = await confirmDialog(
      promote
        ? {
            title: "Make administrator?",
            message: `${user.username} will be able to create and disable accounts and reset anyone's password.`,
            confirmLabel: "Make administrator",
          }
        : {
            title: "Remove administrator rights?",
            message: isSelf
              ? "You won't be able to open this page afterwards."
              : `${user.username} will become a regular member.`,
            confirmLabel: "Remove rights",
            danger: isSelf,
          },
    );
    if (!confirmed) {
      return;
    }

    this.perform(async () => {
      await api.patch(`/api/admin/users/${user.id}`, { role: promote ? "admin" : "member" });
      if (isSelf) {
        // 已經不是管理員：交給上層重新決定畫面，不重新載入這個列表（只會得到 403）
        this.onSelfChanged();
        return false;
      }
      return true;
    });
  }

  private async toggleDisabled(user: ManagedUser): Promise<void> {
    if (
      !user.disabled &&
      !(await confirmDialog({
        title: `Disable ${user.username}?`,
        message: "They'll be logged out on every device and won't be able to log in.",
        confirmLabel: "Disable",
        danger: true,
      }))
    ) {
      return;
    }
    this.perform(() => api.patch(`/api/admin/users/${user.id}`, { disabled: !user.disabled }));
  }

  private async resetPassword(user: ManagedUser): Promise<void> {
    if (
      !(await confirmDialog({
        title: `Reset the password of ${user.username}?`,
        message: "They'll be logged out on every device and need a new temporary password to log in.",
        confirmLabel: "Reset password",
        danger: true,
      }))
    ) {
      return;
    }
    this.perform(async () => {
      const result = await api.post<ResetPasswordResponse>(
        `/api/admin/users/${user.id}/reset-password`,
      );
      this.showPassword(user.username, result.temporary_password, "reset");
    });
  }

  /**
   * 執行一個操作：完成後重新載入列表，失敗則顯示錯誤。
   * action 回傳 false 時不重新載入（例如降級自己之後，已經沒有權限讀取列表）。
   */
  private perform(action: () => Promise<unknown>): void {
    if (this.busy) {
      return;
    }
    this.busy = true;
    this.errorBox.textContent = "";

    void (async () => {
      try {
        const result = await action();
        if (result !== false) {
          await this.loadTable();
        }
      } catch (error) {
        if (!isHandledGlobally(error)) {
          this.errorBox.textContent = messageOf(error);
        }
      } finally {
        this.busy = false;
      }
    })();
  }

  // ---------- 臨時密碼 ----------

  /**
   * 顯示臨時密碼。同時只顯示一組：新的會取代舊的。
   * 「I've noted it」會先清空值，再把整個面板從 DOM 移除。
   */
  private showPassword(username: string, password: string, action: "created" | "reset"): void {
    const field = h("input", {
      type: "text",
      readonly: true,
      value: password,
      class: "temp-password",
      "aria-label": `Temporary password for ${username}`,
      // 不讓密碼管理器把它當成自己的密碼儲存
      autocomplete: "off",
      spellcheck: "false",
    });
    const copyStatus = h("span", { class: "muted", role: "status" });

    const panel = h(
      "div",
      { class: "notice temp-panel", role: "region", "aria-label": "Temporary password" },
      h(
        "p",
        {},
        action === "created" ? "Created " : "Reset the password of ",
        h("strong", {}, username),
        ". This temporary password is shown only once: write it down and pass it on. They must change it when they first log in.",
      ),
      h(
        "div",
        { class: "temp-row" },
        field,
        h("button", { type: "button", onClick: () => void copyPassword(field, copyStatus) }, "Copy"),
        copyStatus,
      ),
      h(
        "button",
        {
          type: "button",
          class: "link",
          onClick: () => {
            // 先清空值，再移除整個面板：關閉後密碼不殘留在 DOM 中
            field.value = "";
            panel.remove();
          },
        },
        "I've noted it, close",
      ),
    );

    this.noticeSlot.replaceChildren(panel);
    field.focus();
    field.select();
  }
}

/**
 * 複製臨時密碼。
 * 剪貼簿 API 只在安全環境（HTTPS 或 localhost）可用；用 http://192.168.x.x 連線時不存在。
 * 不可用或失敗時，選取文字並請使用者自己按複製鍵。
 */
async function copyPassword(field: HTMLInputElement, status: HTMLElement): Promise<void> {
  field.select();
  if (window.isSecureContext && "clipboard" in navigator) {
    try {
      await navigator.clipboard.writeText(field.value);
      status.textContent = "Copied";
      return;
    } catch {
      // 使用者拒絕權限等情況：退回手動複製
    }
  }
  status.textContent = "Selected. Press Ctrl+C (⌘C on a Mac) to copy";
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "Something went wrong";
}
