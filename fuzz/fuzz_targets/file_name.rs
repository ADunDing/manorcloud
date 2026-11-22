//! 檔名檢查：使用者取的名稱（上傳、改名、新資料夾）。通過檢查的名稱會成為硬碟上真正的檔名，
//! 所以必須是單一、普通、看得見的名稱，而且再檢查一次結果相同。
#![no_main]

use std::path::{Component, Path};

use libfuzzer_sys::fuzz_target;

#[path = "../../src/files/name.rs"]
#[allow(dead_code)]
mod name;

fuzz_target!(|text: &str| {
    let Ok(checked) = name::validate_file_name(text) else {
        return;
    };

    assert!(!checked.is_empty());
    assert!(
        checked.len() <= name::MAX_NAME_BYTES,
        "{checked:?} is too long"
    );
    assert!(checked != "." && checked != "..");
    assert!(!checked.contains(['/', '\\']), "separator in {checked:?}");
    assert!(
        !checked.chars().any(char::is_control),
        "control character in {checked:?}"
    );
    assert_eq!(
        checked.trim(),
        checked,
        "surrounding whitespace in {checked:?}"
    );

    // 放進路徑時只是一個普通的名稱，不會變成上一層或根目錄
    let mut components = Path::new(&checked).components();
    assert!(
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
        "{checked:?} is not a single plain name"
    );

    // 已經正規化過：再檢查一次，結果不變
    assert_eq!(
        name::validate_file_name(&checked).as_deref(),
        Ok(checked.as_str())
    );

    // 同名時加上編號的名稱，如果再通過檢查，也同樣安全
    if let Ok(numbered) = name::validate_file_name(&name::numbered_name(&checked, 7)) {
        assert!(!numbered.contains(['/', '\\']));
        assert!(numbered != "." && numbered != "..");
    }
});
