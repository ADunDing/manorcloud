use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

/// token 的隨機位元組數（32 bytes = 256 位元）
const TOKEN_BYTES: usize = 32;

/// 產生一個密碼學安全的隨機 token，以 URL-safe base64 編碼（可直接放進 Cookie）
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];

    // 作業系統的安全亂數來源失敗代表系統出了嚴重問題，
    // 此時繼續執行反而危險，所以直接 panic
    getrandom::fill(&mut buf).expect("failed to get secure random bytes from the OS");

    URL_SAFE_NO_PAD.encode(buf)
}

/// 將 token 雜湊成 32 bytes 的 SHA-256 值，用於存入資料庫（BLOB 欄位）
pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_has_expected_length() {
        // 32 bytes 經 base64 無 padding 編碼後固定是 43 個字元
        assert_eq!(generate_token().len(), 43);
    }

    #[test]
    fn token_is_cookie_safe() {
        // 只能包含 A-Z a-z 0-9 - _
        let token = generate_token();
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn tokens_are_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_deterministic() {
        let token = generate_token();
        assert_eq!(hash_token(&token), hash_token(&token));
    }

    #[test]
    fn different_tokens_different_hashes() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(hash_token(&a), hash_token(&b));
    }

    #[test]
    fn hash_is_32_bytes() {
        assert_eq!(hash_token(&generate_token()).len(), 32);
    }
}
