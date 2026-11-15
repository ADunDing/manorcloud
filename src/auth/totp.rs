//! 兩步驟驗證的計算：TOTP 驗證碼（RFC 6238）、備用碼、設定用的 QR code。
//!
//! 只有計算，不碰資料庫、不讀系統時間：「現在」由呼叫端傳入，測試可以控制時間，
//! 也可以用 RFC 的官方測試資料驗證。
//!
//! TOTP：密鑰與「現在是第幾個 30 秒」一起做 HMAC-SHA1，取出 6 位數。
//! 手機的驗證器 App 與伺服器各自計算，數字相同就代表手機上有這把密鑰。
//! SHA-1 在這裡是 HMAC 的一部分，沒有已知的弱點；所有驗證器 App 都只保證支援 SHA-1。

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// 每個驗證碼的有效時間（秒）
pub const STEP_SECONDS: i64 = 30;
/// 驗證碼的位數
pub const DIGITS: u32 = 6;
/// 允許前後各一個時間段的誤差：手機與伺服器的時間差在 30 秒內都能通過
const SKEW_STEPS: i64 = 1;
/// 驗證失敗時，檢查驗證碼是否與相差較遠的時間相符（前後 12 小時），用來提示伺服器時鐘不準
const CLOCK_CHECK_STEPS: i64 = 12 * 60 * 2;
/// 密鑰長度：160 位元（RFC 4226 建議的長度）
pub const SECRET_BYTES: usize = 20;
/// 備用碼的數量
pub const RECOVERY_CODES: usize = 10;
/// 備用碼使用的字元：小寫英文與數字，去掉容易看錯的 0 o 1 l i
const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

// ==================== 密鑰 ====================

/// 產生新的密鑰
pub fn generate_secret() -> Vec<u8> {
    let mut secret = vec![0u8; SECRET_BYTES];
    getrandom::fill(&mut secret).expect("failed to get secure random bytes from the OS");
    secret
}

/// 驗證器 App 掃描的網址：otpauth://totp/{發行者}:{帳號}?secret=…&issuer=…
///
/// 帳號名稱只有小寫英文、數字、底線、連字號（帳號規則），不需要額外編碼；
/// 發行者固定是 ManorCloud。App 中會顯示成「ManorCloud (alston)」
pub fn otpauth_uri(username: &str, secret: &[u8]) -> String {
    format!(
        "otpauth://totp/ManorCloud:{username}?secret={}&issuer=ManorCloud&algorithm=SHA1&digits={DIGITS}&period={STEP_SECONDS}",
        base32(secret)
    )
}

/// RFC 4648 的 Base32（沒有補位的 =）：驗證器 App 讀取密鑰的格式。
/// 手動輸入時每 4 個字元一組比較好讀，顯示時由前端分組
pub fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

// ==================== 驗證碼 ====================

/// 某個時間段的驗證碼（RFC 4226 的 HOTP，以時間段作為計數器）
pub fn code_at(secret: &[u8], step: i64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(&step.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    // 動態截斷：最後一個位元組的低 4 位元決定從哪裡取 4 個位元組
    let offset = usize::from(hash[hash.len() - 1] & 0x0f);
    let value = u32::from_be_bytes([
        hash[offset] & 0x7f,
        hash[offset + 1],
        hash[offset + 2],
        hash[offset + 3],
    ]);
    value % 10u32.pow(DIGITS)
}

/// 某個時間點屬於第幾個時間段
pub fn step_at(unix_seconds: i64) -> i64 {
    unix_seconds.div_euclid(STEP_SECONDS)
}

/// 驗證使用者輸入的驗證碼。通過時回傳它所屬的時間段，呼叫端要記下來：
/// `last_used_step` 之前（含）的時間段一律不接受，同一個驗證碼不能用第二次
/// （防止被旁人看到、或被攔截之後重複使用）。
///
/// 輸入允許空白（App 顯示成「123 456」）；不是 6 位數字就直接不通過
pub fn verify(secret: &[u8], input: &str, now: i64, last_used_step: i64) -> Option<i64> {
    let digits: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != DIGITS as usize || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let entered: u32 = digits.parse().ok()?;
    let current = step_at(now);
    // 從較早的時間段開始比對：同時符合時記下較早的，下一個驗證碼仍然可以使用。
    // 整數的比較是一個指令，所花的時間與內容無關，不能從回應時間一位一位地猜
    (current - SKEW_STEPS..=current + SKEW_STEPS)
        .filter(|step| *step > last_used_step)
        .find(|step| code_at(secret, *step) == entered)
}

/// 驗證碼不正確時呼叫：它是否與相差較遠的時間相符（12 小時內）。
/// 相符時回傳伺服器的時間差了幾分鐘（正數：伺服器的時鐘比手機慢）。
///
/// 只用來在日誌中提示管理員檢查時鐘（樹莓派沒有硬體時鐘，開機後要等網路校時），
/// 不影響驗證的結果。隨便猜的 6 位數也有千分之三的機會剛好相符，所以只是「提示」
pub fn clock_offset_minutes(secret: &[u8], input: &str, now: i64) -> Option<i64> {
    let digits: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != DIGITS as usize || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let entered: u32 = digits.parse().ok()?;
    let current = step_at(now);
    (SKEW_STEPS + 1..=CLOCK_CHECK_STEPS)
        .flat_map(|distance| [current + distance, current - distance])
        .find(|step| code_at(secret, *step) == entered)
        .map(|step| (step - current) * STEP_SECONDS / 60)
}

// ==================== 備用碼 ====================

/// 產生一組備用碼，格式 xxxx-xxxx（約 40 位元的隨機性；猜的次數受登入次數限制保護）
pub fn generate_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODES)
        .map(|_| {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).expect("failed to get secure random bytes from the OS");
            let chars: String = bytes
                .iter()
                .map(|b| char::from(RECOVERY_ALPHABET[usize::from(*b) % RECOVERY_ALPHABET.len()]))
                .collect();
            format!("{}-{}", &chars[..4], &chars[4..])
        })
        .collect()
}

/// 備用碼的雜湊：資料庫只存它。輸入時大小寫、空白與連字號都不影響
pub fn hash_recovery_code(input: &str) -> Vec<u8> {
    let normalized: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    Sha256::digest(format!("manorcloud-recovery:{normalized}").as_bytes()).to_vec()
}

/// 輸入看起來是備用碼（8 個英數字），而不是 6 位數的驗證碼
pub fn looks_like_recovery_code(input: &str) -> bool {
    input.chars().filter(|c| c.is_ascii_alphanumeric()).count() == 8
}

// ==================== QR code ====================

/// QR code 的點陣：size × size 個點，`modules` 由上而下、由左而右，'1' 是黑點。
/// 前端依此畫出 SVG：不需要外部的 QR 服務，密鑰不會離開這台伺服器
pub struct QrMatrix {
    pub size: usize,
    pub modules: String,
}

pub fn qr_matrix(text: &str) -> QrMatrix {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M)
        .expect("an otpauth URI always fits in a QR code");
    let modules = code
        .to_colors()
        .iter()
        .map(|color| {
            if *color == qrcode::Color::Dark {
                '1'
            } else {
                '0'
            }
        })
        .collect();
    QrMatrix {
        size: code.width(),
        modules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 附錄 B 的 SHA-1 密鑰：ASCII 的 "12345678901234567890"
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_the_rfc_6238_test_vectors() {
        // RFC 的值是 8 位數，取後 6 位就是 6 位數的驗證碼
        for (time, expected) in [
            (59, 94_287_082u32),
            (1_111_111_109, 7_081_804),
            (1_111_111_111, 14_050_471),
            (1_234_567_890, 89_005_924),
            (2_000_000_000, 69_279_037),
            (20_000_000_000, 65_353_130),
        ] {
            assert_eq!(
                code_at(RFC_SECRET, step_at(time)),
                expected % 1_000_000,
                "{time}"
            );
        }
    }

    #[test]
    fn base32_matches_rfc_4648() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"fo"), "MZXQ");
        assert_eq!(base32(b"foo"), "MZXW6");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(RFC_SECRET), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
    }

    #[test]
    fn verification_allows_a_little_clock_skew_only() {
        let now = 1_800_000_000;
        let code = |offset: i64| format!("{:06}", code_at(RFC_SECRET, step_at(now) + offset));

        assert_eq!(verify(RFC_SECRET, &code(0), now, 0), Some(step_at(now)));
        assert!(verify(RFC_SECRET, &code(-1), now, 0).is_some());
        assert!(verify(RFC_SECRET, &code(1), now, 0).is_some());
        assert!(verify(RFC_SECRET, &code(-2), now, 0).is_none());
        assert!(verify(RFC_SECRET, &code(2), now, 0).is_none());
        // App 顯示成 123 456：空白不影響
        let spaced = format!("{} {}", &code(0)[..3], &code(0)[3..]);
        assert!(verify(RFC_SECRET, &spaced, now, 0).is_some());
    }

    #[test]
    fn a_code_cannot_be_used_twice() {
        let now = 1_800_000_000;
        let code = format!("{:06}", code_at(RFC_SECRET, step_at(now)));
        let used = verify(RFC_SECRET, &code, now, 0).unwrap();

        assert!(verify(RFC_SECRET, &code, now, used).is_none());
        // 下一個時間段的新驗證碼可以使用
        let next = format!("{:06}", code_at(RFC_SECRET, step_at(now) + 1));
        assert!(verify(RFC_SECRET, &next, now + 30, used).is_some());
    }

    #[test]
    fn a_wrong_server_clock_is_detected() {
        let now = 1_800_000_000;
        // 手機的時間比伺服器快 10 分鐘
        let phone = format!("{:06}", code_at(RFC_SECRET, step_at(now + 600)));

        assert!(verify(RFC_SECRET, &phone, now, 0).is_none());
        assert_eq!(clock_offset_minutes(RFC_SECRET, &phone, now), Some(10));
    }

    #[test]
    fn malformed_input_never_passes() {
        for input in ["", "12345", "1234567", "abcdef", "12345a", "١٢٣٤٥٦"] {
            assert!(
                verify(RFC_SECRET, input, 1_800_000_000, 0).is_none(),
                "{input}"
            );
        }
    }

    #[test]
    fn otpauth_uri_is_what_authenticator_apps_expect() {
        let uri = otpauth_uri("alston", RFC_SECRET);
        assert!(uri.starts_with("otpauth://totp/ManorCloud:alston?secret=GEZDGNBVGY3TQOJQ"));
        assert!(uri.contains("issuer=ManorCloud"));
        assert!(uri.contains("digits=6") && uri.contains("period=30"));
    }

    #[test]
    fn recovery_codes_are_distinct_and_forgiving_to_type() {
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODES);
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), RECOVERY_CODES);
        for code in &codes {
            assert_eq!(code.len(), 9);
            assert!(looks_like_recovery_code(code));
            // 大寫、沒有連字號、多了空白：都是同一個
            let typed = code.to_uppercase().replace('-', " ");
            assert_eq!(hash_recovery_code(&typed), hash_recovery_code(code));
        }
        assert!(!looks_like_recovery_code("123456"));
    }

    #[test]
    fn qr_matrix_is_square() {
        let qr = qr_matrix(&otpauth_uri("alston", &generate_secret()));
        assert_eq!(qr.modules.len(), qr.size * qr.size);
        assert!(qr.modules.bytes().all(|b| b == b'0' || b == b'1'));
    }
}
