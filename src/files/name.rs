use std::fmt;

use unicode_normalization::UnicodeNormalization;

/// 檔名上限（位元組）。主流檔案系統以位元組限制長度，
/// 超過的名稱在使用者下載回本機時會出問題。
pub const MAX_NAME_BYTES: usize = 255;

/// 檔名不合法的原因
#[derive(Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    DotName,
    Separator,
    ControlChar,
    BidiControl,
    SurroundingWhitespace,
    TooLong,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            NameError::Empty => "名稱不能為空".to_string(),
            NameError::DotName => "名稱不能是 . 或 ..".to_string(),
            NameError::Separator => "名稱不能包含 / 或 \\".to_string(),
            NameError::ControlChar => "名稱不能包含控制字元".to_string(),
            NameError::BidiControl => "名稱不能包含文字方向控制字元".to_string(),
            NameError::SurroundingWhitespace => "名稱開頭與結尾不能有空白".to_string(),
            NameError::TooLong => format!("名稱不能超過 {MAX_NAME_BYTES} 位元組"),
        };
        f.write_str(&msg)
    }
}

impl std::error::Error for NameError {}

/// 驗證並正規化檔名，成功時回傳應存入資料庫的名稱。
///
/// 名稱只存在資料庫、永遠不會進入檔案系統路徑，所以這裡防的是
/// 「看不見的差異」與「破壞顯示或協定」的問題，而不是路徑穿越。
///
/// 刻意不限制：Windows 保留名稱（CON 等）、Windows 禁用字元（: ? * 等）、
/// 結尾的點。它們在伺服器上無害，瀏覽器下載時會自行處理；
/// 擋掉反而讓「會議 10:30.docx」這類常見名稱無法使用。
pub fn validate_file_name(name: &str) -> Result<String, NameError> {
    // 1. Unicode 正規化為 NFC：macOS 的「e + 重音符」與其他系統的「é」統一成同一形式，
    //    避免同一資料夾出現兩個看起來一模一樣、位元組卻不同的檔名
    let name: String = name.nfc().collect();

    if name.is_empty() {
        return Err(NameError::Empty);
    }

    if name == "." || name == ".." {
        return Err(NameError::DotName);
    }

    if name.contains(['/', '\\']) {
        return Err(NameError::Separator);
    }

    // 包含 NUL、換行、DEL 與 C1 控制字元
    if name.chars().any(char::is_control) {
        return Err(NameError::ControlChar);
    }

    if name.chars().any(is_bidi_control) {
        return Err(NameError::BidiControl);
    }

    // trim 以 Unicode 空白為準，全形空白也包含在內
    if name.trim() != name {
        return Err(NameError::SurroundingWhitespace);
    }

    // 長度以正規化後的位元組計
    if name.len() > MAX_NAME_BYTES {
        return Err(NameError::TooLong);
    }

    Ok(name)
}

/// 同名時產生的替代名稱：「旅行.jpg」→「旅行 (1).jpg」。
///
/// 與瀏覽器下載同名檔案的規則相同：只把最後一個點之後當作副檔名；
/// 開頭的點是名稱的一部分（.bashrc → .bashrc (1)）。
/// 回傳值仍需經過 validate_file_name：加上編號後可能超過長度上限。
pub fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// 雙向文字控制字元。可用來偽裝副檔名，例如讓 "invoice\u{202E}fdp.exe"
/// 顯示成 "invoiceexe.pdf"。它們不屬於控制字元分類，需要另外判斷。
/// 零寬連接符（U+200D）不在此列：emoji 組合需要它。
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'              // 阿拉伯字母標記
            | '\u{200E}'        // 左至右標記
            | '\u{200F}'        // 右至左標記
            | '\u{202A}'..='\u{202E}' // 嵌入與覆寫
            | '\u{2066}'..='\u{2069}' // 隔離
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for name in [
            "report.pdf",
            ".gitignore",
            "會議紀錄.docx",
            "會議 10:30.docx",
            "CON",
            "trailing.",
            "README",
            "👨\u{200D}👩\u{200D}👧.jpg", // 含零寬連接符的 emoji
        ] {
            assert_eq!(validate_file_name(name).as_deref(), Ok(name), "{name}");
        }
    }

    #[test]
    fn empty_and_dot_names() {
        assert_eq!(validate_file_name(""), Err(NameError::Empty));
        assert_eq!(validate_file_name("."), Err(NameError::DotName));
        assert_eq!(validate_file_name(".."), Err(NameError::DotName));
        // 三個點是合法名稱
        assert!(validate_file_name("...").is_ok());
    }

    #[test]
    fn path_separators() {
        for name in ["a/b", "../etc/passwd", "/etc/passwd", "a\\b", "..\\windows"] {
            assert_eq!(
                validate_file_name(name),
                Err(NameError::Separator),
                "{name}"
            );
        }
    }

    #[test]
    fn control_characters() {
        for name in ["a\0b", "a\nb", "a\rb", "a\tb", "a\u{7F}b", "a\u{85}b"] {
            assert_eq!(
                validate_file_name(name),
                Err(NameError::ControlChar),
                "{name:?}"
            );
        }
    }

    #[test]
    fn bidi_override_spoofing() {
        assert_eq!(
            validate_file_name("invoice\u{202E}fdp.exe"),
            Err(NameError::BidiControl)
        );
        assert_eq!(
            validate_file_name("a\u{2066}b"),
            Err(NameError::BidiControl)
        );
    }

    #[test]
    fn surrounding_whitespace() {
        for name in [" a", "a ", " ", "a\u{3000}"] {
            assert_eq!(
                validate_file_name(name),
                Err(NameError::SurroundingWhitespace),
                "{name:?}"
            );
        }
        // 中間的空白沒問題
        assert!(validate_file_name("my file.txt").is_ok());
    }

    #[test]
    fn length_is_measured_in_bytes() {
        assert!(validate_file_name(&"a".repeat(255)).is_ok());
        assert_eq!(
            validate_file_name(&"a".repeat(256)),
            Err(NameError::TooLong)
        );

        // 中文每字 3 位元組：85 字 = 255 位元組
        assert!(validate_file_name(&"中".repeat(85)).is_ok());
        assert_eq!(
            validate_file_name(&"中".repeat(86)),
            Err(NameError::TooLong)
        );
    }

    #[test]
    fn unicode_is_normalized_to_nfc() {
        let decomposed = "caf\u{0065}\u{0301}.txt"; // e + 重音符（macOS 的形式）
        let composed = "caf\u{00E9}.txt"; // é 單一碼位

        assert_ne!(decomposed, composed); // 原始位元組不同
        assert_eq!(validate_file_name(decomposed).unwrap(), composed);
        assert_eq!(validate_file_name(composed).unwrap(), composed);
    }

    #[test]
    fn numbered_names() {
        assert_eq!(numbered_name("旅行.jpg", 1), "旅行 (1).jpg");
        assert_eq!(numbered_name("旅行.jpg", 12), "旅行 (12).jpg");
        assert_eq!(numbered_name("README", 1), "README (1)");
        // 開頭的點是名稱的一部分，不是副檔名
        assert_eq!(numbered_name(".bashrc", 1), ".bashrc (1)");
        // 只把最後一個點之後當作副檔名，與瀏覽器相同
        assert_eq!(numbered_name("archive.tar.gz", 1), "archive.tar (1).gz");
    }
}
