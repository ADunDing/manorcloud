// GET /api/files 的回應 → 列表用的 FileItem。
//
// 項目以名稱識別（硬碟上真實的檔名）；FileList 需要數字編號來追蹤選取，
// 這裡用在這次回應中的順序當作編號。重新載入後編號會改變，所以選取不會跨越重新載入。

import type { ApiItem, FileItem } from "./types.js";

export function toFileItems(items: readonly ApiItem[]): FileItem[] {
  return items.map((item, index) => ({
    id: index + 1,
    parent_id: null,
    name: item.name,
    kind: item.kind,
    size: item.size,
    mime: null,
    created_at: item.modified,
    updated_at: item.modified,
  }));
}

/** a 是不是 b 本身，或在 b 底下 */
export function isInside(a: readonly string[], b: readonly string[]): boolean {
  return b.length <= a.length && b.every((part, index) => a[index] === part);
}
