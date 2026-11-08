use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// 403 的原因。
///
/// 同一個狀態碼有好幾種意思，前端要依原因決定行為（例如「請先修改密碼」要顯示修改畫面，
/// 其他只顯示訊息）。原因由型別決定：回 403 時不可能不帶原因，也不可能寫錯代碼。
///
/// `code()` 的字串是與前端的約定，**不可隨意更改**；`message()` 的文案則可以自由調整。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forbidden {
    /// 跨站請求（Origin 與 Host 不符）
    CrossOrigin,
    /// 帳號已停用（只在身分驗證成功之後回傳）
    AccountDisabled,
    /// 密碼是臨時的，必須先修改
    PasswordChangeRequired,
    /// 修改憑證時，目前的密碼不正確
    WrongCurrentPassword,
    /// 需要管理員權限
    AdminRequired,
}

impl Forbidden {
    /// 給程式判斷的代碼
    pub fn code(self) -> &'static str {
        match self {
            Forbidden::CrossOrigin => "cross_origin",
            Forbidden::AccountDisabled => "account_disabled",
            Forbidden::PasswordChangeRequired => "password_change_required",
            Forbidden::WrongCurrentPassword => "wrong_current_password",
            Forbidden::AdminRequired => "admin_required",
        }
    }

    /// 給人看的訊息
    pub fn message(self) -> &'static str {
        match self {
            Forbidden::CrossOrigin => "請求來源不被允許",
            Forbidden::AccountDisabled => "帳號已停用，請聯絡管理員",
            Forbidden::PasswordChangeRequired => "請先修改密碼",
            Forbidden::WrongCurrentPassword => "目前的密碼不正確",
            Forbidden::AdminRequired => "需要管理員權限",
        }
    }
}

/// API 共用的錯誤型別，每種錯誤對應一個 HTTP 狀態碼。
///
/// 回應格式：`{"error": "給人看的訊息", "code": "給程式判斷的代碼"}`
#[derive(Debug)]
pub enum AppError {
    /// 400：輸入格式不符規則
    BadRequest(String),
    /// 401：未通過身分驗證
    Unauthorized(String),
    /// 403：身分已確認，但不被允許；原因見 `Forbidden`
    Forbidden(Forbidden),
    /// 404：找不到資源。也用於「存在但不屬於你」，避免洩漏他人資料是否存在
    NotFound(String),
    /// 409：資源衝突，例如帳號重複、同名檔案
    Conflict(String),
    /// 413：請求內容超過大小上限
    PayloadTooLarge(String),
    /// 500：伺服器內部錯誤，細節只記在 log，不回給客戶端
    Internal,
    /// 507：伺服器儲存空間不足
    InsufficientStorage(String),
}

impl AppError {
    /// 記錄內部錯誤的細節，對外只回傳通用訊息
    pub fn internal(e: impl std::fmt::Display) -> Self {
        eprintln!("internal error: {e}");
        AppError::Internal
    }

    /// 給程式判斷的代碼。403 依原因細分；其他狀態碼目前前端不需要細分，使用通用代碼。
    /// 這些字串是與前端的約定，不可隨意更改。
    pub fn code(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "bad_request",
            AppError::Unauthorized(_) => "unauthorized",
            AppError::Forbidden(reason) => reason.code(),
            AppError::NotFound(_) => "not_found",
            AppError::Conflict(_) => "conflict",
            AppError::PayloadTooLarge(_) => "payload_too_large",
            AppError::Internal => "internal",
            AppError::InsufficientStorage(_) => "insufficient_storage",
        }
    }
}

/// 讓 handler 裡可以直接對 sqlx 的結果用 `?`
impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::internal(e)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: &'static str,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let code = self.code();
        let (status, message) = match self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            AppError::Forbidden(reason) => (StatusCode::FORBIDDEN, reason.message().to_string()),
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            AppError::Conflict(m) => (StatusCode::CONFLICT, m),
            AppError::PayloadTooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m),
            AppError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "伺服器內部錯誤".to_string(),
            ),
            AppError::InsufficientStorage(m) => (StatusCode::INSUFFICIENT_STORAGE, m),
        };

        (
            status,
            Json(ErrorBody {
                error: message,
                code,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 所有 403 原因。新增原因時，下面的 `position` 會因為 match 缺少分支而編譯失敗，
    /// 強迫回來更新這份清單，測試才不會漏掉新的原因。
    const ALL_FORBIDDEN: [Forbidden; 5] = [
        Forbidden::CrossOrigin,
        Forbidden::AccountDisabled,
        Forbidden::PasswordChangeRequired,
        Forbidden::WrongCurrentPassword,
        Forbidden::AdminRequired,
    ];

    /// 每個原因在 ALL_FORBIDDEN 中的位置。刻意不寫 `_` 萬用分支
    fn position(reason: Forbidden) -> usize {
        match reason {
            Forbidden::CrossOrigin => 0,
            Forbidden::AccountDisabled => 1,
            Forbidden::PasswordChangeRequired => 2,
            Forbidden::WrongCurrentPassword => 3,
            Forbidden::AdminRequired => 4,
        }
    }

    async fn body_of(error: AppError) -> (StatusCode, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn all_forbidden_list_is_complete() {
        // 清單的每個位置都對應到 position 中的同一個原因：沒有遺漏，也沒有重複
        for (index, reason) in ALL_FORBIDDEN.iter().enumerate() {
            assert_eq!(position(*reason), index, "{reason:?}");
        }
    }

    #[tokio::test]
    async fn forbidden_has_specific_code_and_message() {
        let (status, body) = body_of(AppError::Forbidden(Forbidden::PasswordChangeRequired)).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "password_change_required");
        assert_eq!(body["error"], "請先修改密碼");
    }

    #[tokio::test]
    async fn other_errors_have_generic_codes() {
        let (status, body) = body_of(AppError::NotFound("找不到資料夾".to_string())).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
        assert_eq!(body["error"], "找不到資料夾");
    }

    #[tokio::test]
    async fn internal_error_hides_details() {
        let (status, body) = body_of(AppError::internal("secret path /srv/data")).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["code"], "internal");
        assert!(!body["error"].as_str().unwrap().contains("srv"));
    }

    #[test]
    fn forbidden_codes_are_unique_and_snake_case() {
        let codes: HashSet<&str> = ALL_FORBIDDEN.iter().map(|f| f.code()).collect();

        // 每個原因的代碼都不同，前端才能分辨
        assert_eq!(codes.len(), ALL_FORBIDDEN.len());
        for code in codes {
            assert!(
                code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{code}"
            );
        }
    }
}
