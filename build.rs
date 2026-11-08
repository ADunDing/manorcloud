// Cargo 在編譯 Rust 程式之前會先執行這個檔案。
// 前端的 JS 是由 TypeScript 編譯產生、不在 git 裡；
// 如果還沒編譯就建置，執行檔裡會沒有任何 JS，網頁永遠停在「載入中…」。
// 這裡在編譯時就擋下這種情況，而不是等部署後才發現。

use std::path::Path;

fn main() {
    let main_js = "frontend/public/js/main.js";

    // 前端重新編譯後，讓 Cargo 重新執行這個檢查
    println!("cargo:rerun-if-changed={main_js}");

    if !Path::new(main_js).exists() {
        panic!(
            "\n\n{main_js} not found.\n\
             Build the frontend first:\n\
             \n    npm --prefix frontend ci && npm --prefix frontend run build\n\n"
        );
    }
}
