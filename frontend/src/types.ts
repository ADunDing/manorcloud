// API 回應的型別。
//
// 注意：這些型別是「我們相信伺服器會回傳這個形狀」，TypeScript 不會在執行時檢查。
// 欄位名稱必須與 Rust 端的回應型別保持一致。

/** 使用者角色，對應 users.role 的 CHECK 約束 */
export type Role = "admin" | "member";

/**
 * 登入、GET /api/me、POST /api/me/credentials 的回應。
 * 對應 Rust 端 web/auth.rs 的 UserResponse。
 */
export interface User {
  readonly id: number;
  readonly username: string;
  readonly role: Role;
  /** 密碼是臨時的：必須先顯示修改畫面，其他 API 都會回 403 */
  readonly must_change_password: boolean;
}

/**
 * 檔案或資料夾。對應 Rust 端 web/files.rs 的 FileResponse。
 * 時間是 Unix 秒數。
 */
export interface FileItem {
  readonly id: number;
  readonly parent_id: number | null;
  readonly name: string;
  readonly kind: "file" | "folder";
  /** 資料夾為 null */
  readonly size: number | null;
  readonly mime: string | null;
  readonly created_at: number;
  readonly updated_at: number;
}

/** 祖先鏈的一層 */
export interface Crumb {
  readonly id: number;
  readonly name: string;
}

/** GET /api/files/{id} 的回應：項目資訊加上由根目錄往下的祖先鏈（不含自己） */
export interface ItemDetail extends FileItem {
  readonly ancestors: readonly Crumb[];
}

/** GET /api/config 的回應：前端需要知道的伺服器設定 */
export interface ServerConfig {
  /** 單一檔案的上傳上限（位元組） */
  readonly max_upload_bytes: number;
}

/** 使用者管理頁的一列。對應 Rust 端 web/admin.rs 的 UserItem */
export interface ManagedUser {
  readonly id: number;
  readonly username: string;
  readonly role: Role;
  readonly disabled: boolean;
  readonly must_change_password: boolean;
  /** SQLite 的 CURRENT_TIMESTAMP 文字（UTC），例如 "2026-11-01 07:23:59" */
  readonly created_at: string;
}

/** POST /api/admin/users 的回應 */
export interface CreatedUser {
  readonly user: ManagedUser;
  /** 只在這次回應中出現一次 */
  readonly temporary_password: string;
}

/** POST /api/admin/users/{id}/reset-password 的回應 */
export interface ResetPasswordResponse {
  /** 只在這次回應中出現一次 */
  readonly temporary_password: string;
}

/**
 * 錯誤代碼。對應 Rust 端 web/error.rs 的 `AppError::code()` 與 `Forbidden::code()`。
 * 這些字串是前後端的約定，兩邊必須同步修改。
 */
export type ErrorCode =
  // 403 的細分原因
  | "cross_origin"
  | "account_disabled"
  | "password_change_required"
  | "wrong_current_password"
  | "admin_required"
  // 其他狀態碼的通用代碼
  | "bad_request"
  | "unauthorized"
  | "not_found"
  | "conflict"
  | "payload_too_large"
  | "internal"
  | "insufficient_storage"
  // 登入失敗次數過多，暫時鎖定（429）
  | "too_many_attempts"
  // 分段上傳（409）
  | "offset_mismatch"
  | "upload_busy"
  | "upload_damaged"
  | "upload_incomplete"
  | "name_conflict"
  | "too_many_uploads";
