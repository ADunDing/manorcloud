// 使用者管理：建立帳號、指派管理員、停用、重設密碼、刪除、更換儲存位置。
//
// 每一列的操作收在最右邊的「⋯」選單裡：按鈕再多，表格也不會被撐開或換行。
//
// 臨時密碼只存在顯示它的面板元素裡，不放進任何變數或狀態；
// 關閉面板時先清空值再移除元素，不會殘留在 DOM 中。

import { t, tNodes } from "./i18n.js";
import { api, ApiError, isHandledGlobally } from "./api.js";
import { confirmDialog, nameDialog } from "./dialogs.js";
import { showToast } from "./toast.js";
import { h } from "./dom.js";
import { formatSize, formatUtcText } from "./format.js";
import { icon } from "./icons.js";
import { dropdown, menuItem, menuSeparator, type MenuEntry } from "./menu.js";
import type {
  CreatedUser,
  ManagedUser,
  ResetPasswordResponse,
  StorageMove,
  StorageOverview,
  StorageVolume,
  User,
} from "./types.js";

/** 有檔案正在搬移時，多久重新讀取一次進度 */
const MOVE_POLL_MS = 2000;

// 與後端 src/web/auth.rs 的帳號規則一致（同 auth-view.ts）。只用來提示，後端才是唯一的權威：
// 不符合時，後端的錯誤訊息會顯示在對話框裡
const USERNAME_HINT =
  t("3–32 characters: lowercase letters, digits, underscores and hyphens.");

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

/** GET /api/admin/quotas 的一筆 */
interface QuotaItem {
  readonly user_id: number;
  /** null：不限制 */
  readonly limit_bytes: number | null;
  /** 管理員沒有配額，是 null */
  readonly used_bytes: number | null;
}

/** 配額以 GB 輸入：1024³ 位元組，與畫面上顯示的大小一致 */
const GB = 1024 ** 3;

class AdminView {
  /** 臨時密碼面板的位置。與列表分開：重新載入列表時，面板不會被清掉 */
  private readonly noticeSlot = h("div", {});
  private readonly errorBox = h("p", { class: "error", role: "alert" });
  private readonly tableSlot = h("div", {});
  /** 操作進行中，避免重複送出 */
  private busy = false;
  /** 已經開啟 2FA 的使用者 */
  private twoFactor = new Set<number>();
  /** 儲存位置、每個人在哪裡、搬移的進度 */
  private storage: StorageOverview = { volumes: [], users: [], moves: [] };
  /** 每個人的配額與用量 */
  private quotas = new Map<number, QuotaItem>();
  /** 定時重新讀取搬移進度 */
  private pollTimer: number | undefined;

  constructor(
    private readonly container: HTMLElement,
    private readonly me: User,
    private readonly onSelfChanged: () => void,
  ) {}

  mount(): void {
    this.container.replaceChildren(
      h(
        "div",
        { class: "admin-head" },
        h("h2", { class: "section-title" }, t("Users")),
        h(
          "button",
          {
            type: "button",
            class: "primary",
            onClick: () => void this.createUser(),
          },
          icon("plus", 16),
          t("Create user"),
        ),
      ),
      this.noticeSlot,
      this.errorBox,
      this.tableSlot,
    );
    void this.loadTable();
  }

  // ---------- 列表 ----------

  /**
   * 只重新載入列表這一塊，臨時密碼面板保留。
   * 有檔案正在搬移時，每隔一段時間自動重新讀取，顯示最新的進度
   */
  private async loadTable(): Promise<void> {
    window.clearTimeout(this.pollTimer);
    // 第一次載入才顯示 Loading：之後的重新讀取直接換掉，畫面不會閃
    if (this.tableSlot.childElementCount === 0) {
      this.tableSlot.replaceChildren(h("p", { class: "muted" }, t("Loading…")));
    }
    try {
      const [users, twoFactor, storage, quotas] = await Promise.all([
        api.get<ManagedUser[]>("/api/admin/users"),
        api.get<{ enabled_user_ids: number[] }>("/api/admin/two-factor"),
        api.get<StorageOverview>("/api/admin/storage"),
        api.get<QuotaItem[]>("/api/admin/quotas"),
      ]);
      this.quotas = new Map(quotas.map((item) => [item.user_id, item]));
      this.twoFactor = new Set(twoFactor.enabled_user_ids);
      this.storage = storage;
      this.tableSlot.replaceChildren(
        h("div", { class: "users-wrap" }, this.table(users)),
      );
      if (storage.moves.some((move) => move.error === null)) {
        this.pollTimer = window.setTimeout(() => this.poll(), MOVE_POLL_MS);
      }
    } catch (error) {
      if (!isHandledGlobally(error)) {
        this.tableSlot.replaceChildren(
          h("p", { class: "error", role: "alert" }, messageOf(error)),
        );
      }
    }
  }

  /** 定時重新讀取。已經離開這一頁時停止；選單或對話框開著時稍後再試，不打斷操作 */
  private poll(): void {
    if (!this.tableSlot.isConnected) {
      return;
    }
    const interacting =
      document.querySelector("dialog[open], .menu:not([hidden])") !== null;
    if (interacting || this.busy) {
      this.pollTimer = window.setTimeout(() => this.poll(), MOVE_POLL_MS);
      return;
    }
    void this.loadTable();
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
          h("th", { scope: "col" }, t("Username")),
          h("th", { scope: "col" }, t("Role")),
          h("th", { scope: "col" }, t("Status")),
          h("th", { scope: "col" }, t("2FA")),
          h("th", { scope: "col" }, t("Storage")),
          h("th", { scope: "col" }, t("Space used")),
          h("th", { scope: "col" }, t("Created")),
          h(
            "th",
            { scope: "col" },
            h("span", { class: "visually-hidden" }, t("Actions")),
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
      ? t("Disabled")
      : user.must_change_password
        ? t("Waiting for first login")
        : t("Active");

    return h(
      "tr",
      { class: user.disabled ? "disabled-row" : null },
      h(
        "td",
        {},
        user.username,
        isSelf ? h("span", { class: "muted" }, ` ${t("(you)")}`) : null,
      ),
      h(
        "td",
        {},
        h(
          "span",
          { class: `badge ${user.role}` },
          user.role === "admin" ? t("Administrator") : t("Member"),
        ),
      ),
      h("td", {}, status),
      h(
        "td",
        {},
        this.twoFactor.has(user.id)
          ? t("On")
          : h("span", { class: "muted" }, t("Off")),
      ),
      h("td", {}, this.storageCell(user)),
      h("td", { class: "nowrap" }, this.usageCell(user)),
      h("td", { class: "nowrap" }, formatUtcText(user.created_at)),
      h("td", { class: "actions-cell" }, this.rowMenu(user, isSelf)),
    );
  }

  /** 用量與配額。管理員沒有配額 */
  private usageCell(user: ManagedUser): HTMLElement | string {
    const quota = this.quotas.get(user.id);
    if (quota === undefined || quota.used_bytes === null) {
      return h("span", { class: "muted" }, "—");
    }
    if (quota.limit_bytes === null) {
      return formatSize(quota.used_bytes);
    }
    const over = quota.used_bytes >= quota.limit_bytes;
    return h(
      "span",
      { class: over ? "error-text" : null },
      t("{used} of {limit}", { used: formatSize(quota.used_bytes), limit: formatSize(quota.limit_bytes) }),
    );
  }

  /** 設定配額：以 GB 輸入，留空是不限制 */
  private async setQuota(user: ManagedUser): Promise<void> {
    const current = this.quotas.get(user.id)?.limit_bytes ?? null;
    const saved = await nameDialog({
      title: t("Storage limit for {name}", { name: user.username }),
      message: t("The most space their files and trash can use, in GB. Leave it empty for no limit."),
      confirmLabel: t("Save"),
      initial: current === null ? "" : String(Math.round((current / GB) * 100) / 100),
      submit: async (text) => {
        const value = text.trim();
        let bytes: number | null = null;
        if (value !== "") {
          const gb = Number(value.replace(",", "."));
          if (!Number.isFinite(gb) || gb < 0) {
            throw new ApiError(400, t("Enter a number of GB, or leave it empty for no limit"));
          }
          bytes = Math.round(gb * GB);
        }
        await api.put(`/api/admin/users/${user.id}/quota`, { limit_bytes: bytes });
      },
    });
    if (saved) {
      showToast(t("Storage limit saved"));
      void this.loadTable();
    }
  }

  /** 這個人的檔案在哪個位置；搬移中顯示進度，失敗時顯示原因 */
  private storageCell(user: ManagedUser): HTMLElement | string {
    const move = this.moveOf(user);
    if (move === undefined) {
      return this.volumeName(this.volumeOf(user));
    }
    const target = this.volumeName(move.target_volume_id);
    if (move.error !== null) {
      return h(
        "span",
        { class: "error", title: move.error },
        t("Couldn't move to {target}", { target }),
      );
    }
    const fraction =
      move.total_bytes > 0 ? move.moved_bytes / move.total_bytes : 0;
    const fill = h("div", { class: "meter-fill" });
    fill.style.width = `${(fraction * 100).toFixed(1)}%`;
    return h(
      "div",
      {
        class: "move-progress",
        title: t("{done} of {total}", { done: formatSize(move.moved_bytes), total: formatSize(move.total_bytes) }),
      },
      h("span", {}, t("Moving to {target}… {percent}%", { target, percent: Math.floor(fraction * 100) })),
      h("div", { class: "meter" }, fill),
    );
  }

  private moveOf(user: ManagedUser): StorageMove | undefined {
    return this.storage.moves.find((move) => move.user_id === user.id);
  }

  private volumeOf(user: ManagedUser): number | null {
    return (
      this.storage.users.find((entry) => entry.user_id === user.id)
        ?.volume_id ?? null
    );
  }

  private volumeName(id: number | null): string {
    return (
      this.storage.volumes.find((volume) => volume.id === id)?.name ??
      t("Unknown")
    );
  }

  /** 一列最右邊的「⋯」按鈕與它的選單 */
  private rowMenu(user: ManagedUser, isSelf: boolean): HTMLElement {
    const entries: (MenuEntry | "separator")[] = [
      {
        label:
          user.role === "admin" ? t("Remove administrator") : t("Make administrator"),
        icon: "users",
        onSelect: () => void this.toggleRole(user, isSelf),
      },
    ];

    // 後端不允許停用、重設密碼、刪除自己；自己的 2FA 要在 Settings 中用密碼與驗證碼關閉
    if (!isSelf) {
      entries.push(
        {
          label: user.disabled ? t("Enable") : t("Disable"),
          icon: "lock",
          onSelect: () => void this.toggleDisabled(user),
        },
        {
          label: t("Reset password"),
          icon: "key",
          onSelect: () => void this.resetPassword(user),
        },
      );
      if (this.twoFactor.has(user.id)) {
        entries.push({
          label: t("Turn off 2FA"),
          icon: "shield",
          onSelect: () => void this.resetTwoFactor(user),
        });
      }
    }

    // 儲存位置：自己的也可以換
    entries.push(...this.storageEntries(user));
    if (user.role !== "admin") {
      entries.push({
        label: t("Set storage limit…"),
        icon: "disk",
        onSelect: () => void this.setQuota(user),
      });
    }

    if (!isSelf) {
      entries.push("separator", {
        label: t("Delete user"),
        icon: "trash",
        danger: true,
        onSelect: () => void this.deleteUser(user),
      });
    }

    const button = h(
      "button",
      {
        type: "button",
        class: "icon-button row-menu",
        title: t("Actions"),
        "aria-label": t("Actions for {name}", { name: user.username }),
      },
      icon("more", 18),
    );
    const menu = h(
      "div",
      {
        class: "menu dropdown-menu",
        role: "menu",
        "aria-label": t("Actions for {name}", { name: user.username }),
      },
      entries.map((entry) =>
        entry === "separator" ? menuSeparator() : menuItem(entry),
      ),
    );
    // 選單相對於視窗定位：表格在可以左右捲動的區域裡，選單不會被裁切
    dropdown(button, menu, { fixed: true });
    return h("div", { class: "row-menu-wrap" }, button, menu);
  }

  /** 選單中與儲存位置有關的項目：依搬移的狀態而不同 */
  private storageEntries(user: ManagedUser): MenuEntry[] {
    const move = this.moveOf(user);
    if (move === undefined) {
      const onlyOne = this.storage.volumes.length < 2;
      return [
        {
          label: t("Change storage"),
          icon: "disk",
          disabled: onlyOne,
          title: onlyOne
            ? t("Add another storage in Administration → Storage first")
            : undefined,
          onSelect: () => void this.changeStorage(user),
        },
      ];
    }
    const entries: MenuEntry[] = [];
    if (move.error !== null) {
      entries.push({
        label: t("Retry moving files"),
        icon: "restore",
        onSelect: () =>
          this.perform(() =>
            api.post(`/api/admin/users/${user.id}/storage/retry`),
          ),
      });
    }
    entries.push({
      label: t("Stop moving files"),
      icon: "close",
      onSelect: () => void this.cancelMove(user),
    });
    return entries;
  }

  // ---------- 建立 ----------

  /** 先按 Create user，再在對話框中輸入名稱。名稱有問題時錯誤顯示在對話框裡，不必重新輸入 */
  private async createUser(): Promise<void> {
    if (this.busy) {
      return;
    }
    let created: CreatedUser | null = null;
    // 有其他硬碟時：順便選擇他的檔案放在哪裡。會在那裡建立以帳號命名的資料夾
    const { volumes } = this.storage;
    const disk =
      volumes.length < 2
        ? null
        : h(
            "select",
            { class: "dialog-input" },
            volumes.map((volume) =>
              h("option", { value: volume.id === null ? "" : String(volume.id) }, volume.name),
            ),
          );
    await nameDialog({
      title: t("Create user"),
      message: `${t("The new member gets a temporary password and must change it when they first log in.")} ${USERNAME_HINT}`,
      confirmLabel: t("Create"),
      extra:
        disk === null
          ? undefined
          : h(
              "label",
              { class: "dialog-field" },
              t("Keep their files on"),
              disk,
              h("span", { class: "hint" }, t("A folder with their username is created there.")),
            ),
      submit: async (username) => {
        created = await api.post<CreatedUser>("/api/admin/users", { username });
      },
    });
    if (created === null) {
      return;
    }
    const { user, temporary_password } = created as CreatedUser;
    this.showPassword(user.username, temporary_password, "created");
    // 新帳號還沒有任何檔案：換位置會立刻完成，不需要搬移
    if (disk !== null && disk.value !== "") {
      try {
        await api.put(`/api/admin/users/${user.id}/storage`, { volume_id: Number(disk.value) });
      } catch (error) {
        if (!isHandledGlobally(error)) {
          this.errorBox.textContent = t("{name} was created, but their files are kept on Default: {error}", { name: user.username, error: messageOf(error) });
        }
      }
    }
    await this.loadTable();
  }

  // ---------- 修改 ----------

  private async toggleRole(user: ManagedUser, isSelf: boolean): Promise<void> {
    const promote = user.role !== "admin";
    const confirmed = await confirmDialog(
      promote
        ? {
            title: t("Make administrator?"),
            message: t("{name} will be able to create and disable accounts and reset anyone's password.", { name: user.username }),
            confirmLabel: t("Make administrator"),
          }
        : {
            title: t("Remove administrator rights?"),
            message: isSelf
              ? t("You won't be able to open this page afterwards.")
              : t("{name} will become a regular member.", { name: user.username }),
            confirmLabel: t("Remove rights"),
            danger: isSelf,
          },
    );
    if (!confirmed) {
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

  private async toggleDisabled(user: ManagedUser): Promise<void> {
    if (
      !user.disabled &&
      !(await confirmDialog({
        title: t("Disable {name}?", { name: user.username }),
        message:
          t("They'll be logged out on every device and won't be able to log in."),
        confirmLabel: t("Disable"),
        danger: true,
      }))
    ) {
      return;
    }
    this.perform(() =>
      api.patch(`/api/admin/users/${user.id}`, { disabled: !user.disabled }),
    );
  }

  /** 手機與備用碼都遺失時：關閉這個人的 2FA，同時登出他所有的裝置 */
  private async resetTwoFactor(user: ManagedUser): Promise<void> {
    const confirmed = await confirmDialog({
      title: t("Turn off two-factor authentication for {name}?", { name: user.username }),
      message:
        t("They'll be logged out on every device and can log in with just their password. Use this when they've lost their phone and their recovery codes."),
      confirmLabel: t("Turn off 2FA"),
      danger: true,
    });
    if (!confirmed) {
      return;
    }
    this.perform(async () => {
      await api.post(`/api/admin/users/${user.id}/reset-two-factor`);
      showToast(t("Two-factor authentication turned off for {name}", { name: user.username }));
    });
  }

  // ---------- 儲存位置 ----------

  private async changeStorage(user: ManagedUser): Promise<void> {
    const current = this.volumeOf(user);
    const target = await chooseVolume(
      user.username,
      this.storage.volumes,
      current,
    );
    if (target === undefined) {
      return;
    }
    this.perform(async () => {
      const result = await api.put<{ moving: boolean }>(
        `/api/admin/users/${user.id}/storage`,
        { volume_id: target },
      );
      showToast(
        result.moving
          ? t("Moving {name}'s files to {target}", { name: user.username, target: this.volumeName(target) })
          : t("{name}'s files are now kept in {target}", { name: user.username, target: this.volumeName(target) }),
      );
    });
  }

  private async cancelMove(user: ManagedUser): Promise<void> {
    const confirmed = await confirmDialog({
      title: t("Stop moving files?"),
      message: t("{name}'s files stay where they are now. Anything already copied to the new location is cleaned up later.", { name: user.username }),
      confirmLabel: t("Stop moving"),
    });
    if (!confirmed) {
      return;
    }
    this.perform(() =>
      api.delete(`/api/admin/users/${user.id}/storage/move`),
    );
  }

  /** 刪除使用者與他所有的檔案。必須輸入帳號名稱才能確認 */
  private async deleteUser(user: ManagedUser): Promise<void> {
    const confirmed = await confirmDialog({
      title: t("Delete {name}?", { name: user.username }),
      message:
        t("Their account and all of their files, including items in their trash, will be deleted permanently. This can't be undone."),
      confirmLabel: t("Delete user"),
      danger: true,
      typeToConfirm: user.username,
    });
    if (!confirmed) {
      return;
    }
    this.perform(async () => {
      await api.delete(`/api/admin/users/${user.id}`);
      showToast(t("Deleted {name}", { name: user.username }));
    });
  }

  private async resetPassword(user: ManagedUser): Promise<void> {
    if (
      !(await confirmDialog({
        title: t("Reset the password of {name}?", { name: user.username }),
        message:
          t("They'll be logged out on every device and need a new temporary password to log in."),
        confirmLabel: t("Reset password"),
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
  private showPassword(
    username: string,
    password: string,
    action: "created" | "reset",
  ): void {
    const field = h("input", {
      type: "text",
      readonly: true,
      value: password,
      class: "temp-password",
      "aria-label": t("Temporary password for {name}", { name: username }),
      // 不讓密碼管理器把它當成自己的密碼儲存
      autocomplete: "off",
      spellcheck: "false",
    });
    const copyStatus = h("span", { class: "muted", role: "status" });

    const panel = h(
      "div",
      {
        class: "notice temp-panel",
        role: "region",
        "aria-label": t("Temporary password"),
      },
      h(
        "p",
        {},
        action === "created"
          ? tNodes(
              "Created {name}. This temporary password is shown only once: write it down and pass it on. They must change it when they first log in.",
              { name: h("strong", {}, username) },
            )
          : tNodes(
              "Reset the password of {name}. This temporary password is shown only once: write it down and pass it on. They must change it when they first log in.",
              { name: h("strong", {}, username) },
            ),
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
          t("Copy"),
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
        t("I've noted it, close"),
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
      status.textContent = t("Copied");
      return;
    } catch {
      // 使用者拒絕權限等情況：退回手動複製
    }
  }
  status.textContent = t("Selected. Press Ctrl+C (⌘C on a Mac) to copy");
}

/**
 * 選擇新的儲存位置。回傳位置的編號（null 是資料目錄）；取消時回傳 undefined
 */
function chooseVolume(
  username: string,
  volumes: readonly StorageVolume[],
  current: number | null,
): Promise<number | null | undefined> {
  return new Promise((resolve) => {
    let result: number | null | undefined;
    const options = volumes.map((volume) => {
      const free =
        volume.available_bytes === null
          ? t("not available")
          : t("{size} free", { size: formatSize(volume.available_bytes) });
      return h(
        "option",
        {
          value: volume.id === null ? "" : String(volume.id),
          selected: volume.id === current,
          disabled: volume.id === current || volume.available_bytes === null,
        },
        volume.id === current ? t("{name} ({free}, current)", { name: volume.name, free }) : `${volume.name} (${free})`,
      );
    });
    const select = h("select", { class: "dialog-input" }, options);
    // 預設選第一個可以選的，而不是目前的位置
    const firstFree = options.find((option) => !option.disabled);
    if (firstFree !== undefined) {
      firstFree.selected = true;
    }
    const cancel = h("button", { type: "button", class: "secondary" }, t("Cancel"));
    const confirm = h(
      "button",
      { type: "submit", class: "primary", disabled: firstFree === undefined },
      t("Move files"),
    );
    const title = t("Change where {name}'s files are kept", { name: username });
    const form = h(
      "form",
      { method: "dialog", class: "dialog-form" },
      h("h2", { class: "dialog-title" }, title),
      h(
        "p",
        { class: "dialog-message" },
        t("Their files are copied to the new storage in the background. Until it's done they can browse and download, but not upload. Unfinished uploads are cancelled."),
      ),
      h("label", { class: "dialog-field" }, t("Storage"), select),
      h("div", { class: "dialog-actions" }, cancel, confirm),
    );
    const dialog = h("dialog", { class: "dialog", "aria-label": title }, form);

    cancel.addEventListener("click", () => dialog.close());
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      result = select.value === "" ? null : Number(select.value);
      dialog.close();
    });
    dialog.addEventListener("close", () => {
      dialog.remove();
      resolve(result);
    });

    document.body.append(dialog);
    dialog.showModal();
    select.focus();
  });
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : t("Something went wrong");
}
