//! 空間中的相對路徑（`相片/2026/a.jpg`）。
//!
//! 獨立成一個只依賴標準函式庫的模組：解析網址傳來的路徑是防止路徑穿越（`../`）的第一道關卡，
//! fuzz/ 的測試直接引入這個檔案，用大量隨機輸入檢查它。

/// 空間裡的一個相對路徑：一串已經檢查過的名稱。空的代表空間的最上層
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelPath(Vec<String>);

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidPath;

impl RelPath {
    /// 解析 `相片/2026/a.jpg`。前後的 `/` 忽略；空字串是最上層
    pub fn parse(text: &str) -> Result<Self, InvalidPath> {
        let trimmed = text.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(RelPath(Vec::new()));
        }
        let mut parts = Vec::new();
        for part in trimmed.split('/') {
            if part.is_empty()
                || part == "."
                || part == ".."
                || part.contains(['\0', '\\'])
                || part.len() > 255
            {
                return Err(InvalidPath);
            }
            parts.push(part.to_string());
        }
        Ok(RelPath(parts))
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn parts(&self) -> &[String] {
        &self.0
    }

    /// 最後一段的名稱；最上層沒有
    pub fn name(&self) -> Option<&str> {
        self.0.last().map(String::as_str)
    }

    /// 上一層；最上層沒有
    pub fn parent(&self) -> Option<RelPath> {
        (!self.0.is_empty()).then(|| RelPath(self.0[..self.0.len() - 1].to_vec()))
    }

    /// 底下的一個名稱。名稱必須已經檢查過
    pub fn child(&self, name: &str) -> RelPath {
        let mut parts = self.0.clone();
        parts.push(name.to_string());
        RelPath(parts)
    }

    /// 接上另一段相對路徑
    pub fn join(&self, other: &RelPath) -> RelPath {
        let mut parts = self.0.clone();
        parts.extend(other.0.iter().cloned());
        RelPath(parts)
    }

    /// 自己是不是 `other` 本身，或在它底下
    pub fn starts_with(&self, other: &RelPath) -> bool {
        self.0.starts_with(&other.0)
    }

    /// `相片/2026/a.jpg`
    pub fn display(&self) -> String {
        self.0.join("/")
    }
}
