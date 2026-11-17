// 管理員：第一層的 users 資料夾（#/disk/{disk}/users）。
// 列出檔案放在這一顆硬碟上的使用者，點進去瀏覽他的檔案（唯讀，見 browse-view.ts）。

import { api, ApiError, isHandledGlobally } from "./api.js";
import { h } from "./dom.js";
import { icon } from "./icons.js";
import { browseHref, folderHref } from "./router.js";
import type { StorageOverview } from "./types.js";

/** 載入序號：較慢回來的舊結果不能蓋掉新的畫面 */
let loading = 0;

/**
 * @param volumeId 哪一顆硬碟（null 是資料目錄）
 * @param userId   目前的管理員：不列出自己；他的檔案所在的那一顆硬碟叫 My files
 */
export function renderUsers(container: HTMLElement, volumeId: number | null, userId: number): void {
  const token = ++loading;
  container.replaceChildren(h("p", { class: "muted" }, "Loading…"));

  void (async () => {
    try {
      const storage = await api.get<StorageOverview>("/api/admin/storage");
      if (token !== loading) {
        return;
      }
      const volume = storage.volumes.find((v) => v.id === volumeId);
      if (volume === undefined) {
        container.replaceChildren(
          h("p", { class: "error", role: "alert" }, "Storage location not found"),
        );
        return;
      }
      const home = storage.users.find((user) => user.user_id === userId)?.volume_id ?? null;
      const atHome = home === volumeId;
      const people = storage.users.filter(
        (user) => user.volume_id === volumeId && user.user_id !== userId,
      );

      container.replaceChildren(
        h(
          "div",
          { class: "folder-header" },
          h(
            "nav",
            { class: "breadcrumbs", "aria-label": "Current location" },
            h(
              "ol",
              {},
              h(
                "li",
                {},
                h(
                  "a",
                  { href: atHome ? folderHref(null) : folderHref(null, volumeId) },
                  atHome ? "My files" : volume.name,
                ),
              ),
              h("li", {}, h("span", { "aria-current": "page" }, "users")),
            ),
          ),
        ),
        h(
          "div",
          { class: "trash-banner" },
          icon("info", 18),
          h(
            "span",
            {},
            `People whose files are kept on ${volume.name}. You can open and download their files, but not change them.`,
          ),
        ),
        people.length === 0
          ? h("p", { class: "empty-state" }, "Nobody else's files are kept here.")
          : h(
              "ul",
              { class: "disk-folder-grid", "aria-label": "People" },
              people.map((user) =>
                h(
                  "li",
                  {},
                  h(
                    "a",
                    {
                      href: browseHref(user.user_id),
                      class: "disk-folder",
                      title: `Browse ${user.username}'s files`,
                    },
                    icon("folder", 40),
                    h("span", { class: "disk-folder-name" }, user.folder),
                  ),
                ),
              ),
            ),
      );
    } catch (error) {
      if (token !== loading || isHandledGlobally(error)) {
        return;
      }
      container.replaceChildren(h("p", { class: "error", role: "alert" }, messageOf(error)));
    }
  })();
}

function messageOf(error: unknown): string {
  return error instanceof ApiError ? error.message : "Something went wrong";
}
