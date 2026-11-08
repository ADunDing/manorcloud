use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::Error};

/// 將明文密碼雜湊成 PHC 格式字串，可直接存進資料庫
pub fn hash_password(password: &str) -> Result<String, Error> {
    // 1. 建立 Argon2 雜湊器（預設 argon2id）
    let argon2 = Argon2::default();

    // 2. 雜湊（salt 由函式內部自動隨機產生）
    let hash: PasswordHash = argon2.hash_password(password.as_bytes())?;

    // 3. 轉成字串（內含演算法、參數、salt、雜湊值）
    Ok(hash.to_string())
}

/// 驗證明文密碼是否與資料庫中的雜湊字串相符
pub fn verify_password(password: &str, hash: &str) -> bool {
    // 1. 解析雜湊字串；格式損壞就直接視為驗證失敗
    let parsed = match PasswordHash::new(hash) {
        Ok(p) => p,
        Err(_) => return false,
    };

    // 2. 用相同的 salt 和參數重算並比對
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_works() {
        let h = hash_password("mypassword123").unwrap();
        assert!(h.starts_with("$argon2id$"));
    }

    #[test]
    fn same_password_different_hash() {
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn verify_correct_password() {
        let h = hash_password("correct").unwrap();
        assert!(verify_password("correct", &h));
    }

    #[test]
    fn verify_wrong_password() {
        let h = hash_password("correct").unwrap();
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn verify_invalid_hash_string() {
        // 資料庫內容損壞時不應 panic，只回傳 false
        assert!(!verify_password("anything", "fake_hash"));
    }
}
