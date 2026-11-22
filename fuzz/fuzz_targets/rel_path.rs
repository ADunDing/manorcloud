//! 網址中的路徑（例如 ?path=相片/2026）：解析成功的路徑，接在空間的根目錄後面，
//! 絕對不能跑到根目錄之外。這是防止路徑穿越（../）的第一道關卡。
#![no_main]

use std::path::{Component, Path};

use libfuzzer_sys::fuzz_target;

#[path = "../../src/relpath.rs"]
#[allow(dead_code)]
mod relpath;

use relpath::RelPath;

fuzz_target!(|text: &str| {
    let Ok(rel) = RelPath::parse(text) else {
        return;
    };

    let root = Path::new("/srv/space");
    let mut joined = root.to_path_buf();
    for part in rel.parts() {
        // 每一段都是一個普通的名稱：不是 . 或 ..，不含分隔符號，也不是空的
        assert!(!part.is_empty(), "empty part in {text:?}");
        assert!(part != "." && part != "..", "dot part in {text:?}");
        assert!(!part.contains(['/', '\\', '\0']), "separator in {text:?}");
        let mut components = Path::new(part).components();
        assert!(
            matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
            "{part:?} from {text:?} is not a single plain name"
        );
        joined.push(part);
    }

    // 接起來的結果一定在根目錄底下
    assert!(
        joined.starts_with(root),
        "{joined:?} escaped the root for {text:?}"
    );
    assert!(
        joined
            .components()
            .all(|c| !matches!(c, Component::ParentDir)),
        "{joined:?} contains .. for {text:?}"
    );

    // 顯示出來再解析一次，得到同一個路徑
    assert_eq!(
        RelPath::parse(&rel.display()).as_ref(),
        Ok(&rel),
        "{text:?}"
    );
});
