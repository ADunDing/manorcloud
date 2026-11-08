// 使用者管理：建立帳號、指派管理員、停用、重設密碼。
//
// 臨時密碼只存在顯示它的面板元素裡，不放進任何變數或狀態；
// 關閉面板時先清空值再移除元素，不會殘留在 DOM 中。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { formatUtcText } from "./format.js";
import type {
  CreatedUser,
  ManagedUser,
  ResetPasswordResponse,
  User,
} from "./types.js";

// 與後端 src/web/auth.rs 的帳號規則一致（同 auth-view.ts）。後端才是唯一的權威
const USERNAME_MIN = 3;
const USERNAME_MAX = 32;
const USERNAME_PATTERN = "[a-z0-9_\\-]+";
const USERNAME_HINT = `${USERNAME_MIN}～${USERNAME_MAX} 個字元，只能使用小寫英文、數字、底線與連字號`;

/**
 * 在 container 中顯示使用者管理頁。
 *
 * @param me            目前登入的管理員
 * @param onSelfChanged 修改了自己的角色之後呼叫（例如降級為成員，已經不能再看這一頁）
 */
export function renderAdmin(
  container: HTMLElement,
  me: User,
  onSelfChanged: () => void,
): void {
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
      h("h1", { class: "page-title" }, "使用者管理"),
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
    this.tableSlot.replaceChildren(h("p", { class: "muted" }, "載入中…"));
    try {
      const users = await api.get<ManagedUser[]>("/api/admin/users");
      this.tableSlot.replaceChildren(this.table(users));
    } catch (error) {
      if (!isHandledGlobally(error)) {
        this.tableSlot.replaceChildren(
          h("p", { class: "error", role: "alert" }, messageOf(error)),
        );
      }
    }
  }

  private table(users: readonly ManagedUser[]): HTMLElement {
    return h(
      "table",
      { class: "files" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          h("th", { scope: "col" }, "帳號"),
          h("th", { scope: "col" }, "角色"),
          h("th", { scope: "col" }, "狀態"),
          h("th", { scope: "col" }, "建立時間"),
          h(
            "th",
            { scope: "col" },
            h("span", { class: "visually-hidden" }, "操作"),
          ),
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
      ? "已停用"
      : user.must_change_password
        ? "待設定密碼"
        : "使用中";

    return h(
      "tr",
      { class: user.disabled ? "disabled-row" : null },
      h(
        "td",
        { class: "name" },
        user.username,
        isSelf ? h("span", { class: "muted" }, "（你）") : null,
      ),
      h(
        "td",
        {},
        h(
          "span",
          { class: `badge ${user.role}` },
          user.role === "admin" ? "管理員" : "成員",
        ),
      ),
      h("td", {}, status),
      h("td", {}, formatUtcText(user.created_at)),
      h(
        "td",
        { class: "actions-cell" },
        h(
          "button",
          {
            type: "button",
            class: "link",
            onClick: () => this.toggleRole(user, isSelf),
          },
          user.role === "admin" ? "取消管理員" : "設為管理員",
        ),
        // 後端不允許停用自己、重設自己的密碼：自己那一列不顯示這兩個按鈕
        isSelf
          ? null
          : h(
              "button",
              {
                type: "button",
                class: "link",
                onClick: () => this.toggleDisabled(user),
              },
              user.disabled ? "啟用" : "停用",
            ),
        isSelf
          ? null
          : h(
              "button",
              {
                type: "button",
                class: "link",
                onClick: () => this.resetPassword(user),
              },
              "重設密碼",
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
      h("label", {}, "新帳號名稱", input),
      h("button", { type: "submit" }, "建立帳號"),
    );

    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const username = input.value;
      this.perform(async () => {
        const created = await api.post<CreatedUser>("/api/admin/users", {
          username,
        });
        input.value = "";
        this.showPassword(
          created.user.username,
          created.temporary_password,
          "建立",
        );
      });
    });

    return form;
  }

  // ---------- 修改 ----------

  private toggleRole(user: ManagedUser, isSelf: boolean): void {
    const promote = user.role !== "admin";
    // confirm 的文字是純文字，帳號名稱不會被當成 HTML 解析
    const message = promote
      ? `確定要讓「${user.username}」成為管理員嗎？管理員可以建立帳號、停用帳號、重設任何人的密碼。`
      : isSelf
        ? "確定要取消自己的管理員身分嗎？取消後將無法再進入這一頁。"
        : `確定要取消「${user.username}」的管理員身分嗎？`;
    if (!window.confirm(message)) {
      return;
    }

    this.perform(async () => {
      await api.patch(`/api/admin/users/${user.id}`, {
        role: promote ? "admin" : "member",
      });
      if (isSelf) {
        // 已經不是管理員：交給上層重新決定畫面，不重新載入這個列表（只會得到 403）
        this.onSelfChanged();
        return false;
      }
      return true;
    });
  }

  private toggleDisabled(user: ManagedUser): void {
    if (
      !user.disabled &&
      !window.confirm(
        `確定要停用「${user.username}」嗎？他將立刻被登出所有裝置，且無法再登入。`,
      )
    ) {
      return;
    }
    this.perform(() =>
      api.patch(`/api/admin/users/${user.id}`, { disabled: !user.disabled }),
    );
  }

  private resetPassword(user: ManagedUser): void {
    if (
      !window.confirm(
        `確定要重設「${user.username}」的密碼嗎？他將立刻被登出所有裝置，並需要用新的臨時密碼登入。`,
      )
    ) {
      return;
    }
    this.perform(async () => {
      const result = await api.post<ResetPasswordResponse>(
        `/api/admin/users/${user.id}/reset-password`,
      );
      this.showPassword(user.username, result.temporary_password, "重設");
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
   * 「我已記下，關閉」會先清空值，再把整個面板從 DOM 移除。
   */
  private showPassword(
    username: string,
    password: string,
    action: "建立" | "重設",
  ): void {
    const field = h("input", {
      type: "text",
      readonly: true,
      value: password,
      class: "temp-password",
      "aria-label": `${username} 的臨時密碼`,
      // 不讓密碼管理器把它當成自己的密碼儲存
      autocomplete: "off",
      spellcheck: "false",
    });
    const copyStatus = h("span", { class: "muted", role: "status" });

    const panel = h(
      "div",
      { class: "notice temp-panel", role: "region", "aria-label": "臨時密碼" },
      h(
        "p",
        {},
        `已${action}「`,
        h("strong", {}, username),
        "」的臨時密碼。這組密碼只會顯示這一次，請現在記下並轉告對方；對方首次登入時必須修改。",
      ),
      h(
        "div",
        { class: "temp-row" },
        field,
        h(
          "button",
          {
            type: "button",
            onClick: () => void copyPassword(field, copyStatus),
          },
          "複製",
        ),
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
        "我已記下，關閉",
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
async function copyPassword(
  field: HTMLInputElement,
  status: HTMLElement,
): Promise<void> {
  field.select();
  if (window.isSecureContext && "clipboard" in navigator) {
    try {
      await navigator.clipboard.writeText(field.value);
      status.textContent = "已複製";
      return;
    } catch {
      // 使用者拒絕權限等情況：退回手動複製
    }
  }
  status.textContent = "已選取，請按 Ctrl+C（Mac 為 ⌘C）複製";
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "發生未預期的錯誤";
}
