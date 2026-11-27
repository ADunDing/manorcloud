//! 串流 tar 寫入器：一邊讀檔案一邊產生 .tar，與 `ZipWriter` 的用法相同。
//!
//! tar 本身只負責打包：要壓縮時，把產生的位元組再交給 gzip 或 zstd（見 `compress`）。
//!
//! 格式的重點（POSIX.1-2001 pax，ustar 的延伸）：
//! - 每個項目：512 位元組的檔頭 → 內容 → 補零到 512 的倍數
//! - 結尾：兩個全零的 512 位元組區塊
//! - ustar 檔頭的欄位很小：名稱 100 位元組、大小 11 位八進位數（最大 8 GiB）。
//!   超過的值放在前面另一個 pax 延伸檔頭（類型 'x'）中，由它覆蓋 ustar 欄位的值。
//!   pax 的名稱一律是 UTF-8，所以中文檔名也一併放進 pax，在各平台都能正確解開
//! - 檔頭的 checksum 是整個檔頭所有位元組的總和，計算時 checksum 欄位本身當成 8 個空白
//!
//! 這個模組只負責產生位元組，不碰磁碟也不碰網路。

use super::archive::ArchiveError;

const BLOCK: usize = 512;

/// ustar 名稱欄位的長度。不使用 prefix 欄位拆開長路徑：長路徑一律放進 pax，規則較單純
const NAME_LEN: usize = 100;
/// 11 位八進位數能表示的最大值：大小與修改時間超過它就放進 pax
const OCTAL11_MAX: u64 = 0o777_7777_7777;

const TYPE_FILE: u8 = b'0';
const TYPE_DIRECTORY: u8 = b'5';
const TYPE_PAX: u8 = b'x';

/// pax 延伸檔頭本身的名稱。它不是真的檔案：支援 pax 的程式會套用它的內容，
/// 不支援的舊程式則會把它解成一個普通檔案，不會弄壞其他項目
const PAX_NAME: &str = "././@PaxHeader";

const FILE_MODE: u64 = 0o644;
const DIRECTORY_MODE: u64 = 0o755;

/// 寫到一半的檔案
struct Pending {
    expected: u64,
    written: u64,
}

/// 串流 tar 寫入器。依序呼叫：
/// `directory` 或 `start_file` → `update`（多次）→ `finish_file`，最後 `finish`。
/// 每個方法回傳的位元組要依序送出；`update` 的資料由呼叫端自己送出（不複製）。
#[derive(Default)]
pub struct TarWriter {
    pending: Option<Pending>,
}

impl TarWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加入一個資料夾（名稱以 / 結尾）。空資料夾也因此會出現在解開的結果中
    pub fn directory(&mut self, path: &str, mtime: i64) -> Result<Vec<u8>, ArchiveError> {
        if self.pending.is_some() {
            return Err(ArchiveError::EntryInProgress);
        }
        let name = format!("{}/", path.trim_end_matches('/'));
        Ok(entry_header(
            &name,
            mtime,
            0,
            TYPE_DIRECTORY,
            DIRECTORY_MODE,
        ))
    }

    /// 開始一個檔案，回傳檔頭。`size` 寫在檔頭中，結束時核對實際讀到的大小
    pub fn start_file(
        &mut self,
        path: &str,
        mtime: i64,
        size: u64,
    ) -> Result<Vec<u8>, ArchiveError> {
        if self.pending.is_some() {
            return Err(ArchiveError::EntryInProgress);
        }
        self.pending = Some(Pending {
            expected: size,
            written: 0,
        });
        Ok(entry_header(path, mtime, size, TYPE_FILE, FILE_MODE))
    }

    /// 檔案的一段內容經過這裡：累計長度。內容本身由呼叫端送出
    pub fn update(&mut self, data: &[u8]) {
        if let Some(pending) = self.pending.as_mut() {
            pending.written += data.len() as u64;
        }
    }

    /// 結束目前的檔案，回傳補到 512 倍數的零。
    ///
    /// 大小與檔頭不符時回傳錯誤：tar 沒有目錄可以事後修正，
    /// 呼叫端必須讓下載失敗，而不是送出一個後面全部錯位的檔案
    pub fn finish_file(&mut self) -> Result<Vec<u8>, ArchiveError> {
        let pending = self.pending.take().ok_or(ArchiveError::NoEntry)?;
        if pending.written != pending.expected {
            return Err(ArchiveError::SizeMismatch {
                expected: pending.expected,
                actual: pending.written,
            });
        }
        Ok(vec![0; padding(pending.written)])
    }

    /// 結束整個檔案：兩個全零的區塊
    pub fn finish(self) -> Result<Vec<u8>, ArchiveError> {
        if self.pending.is_some() {
            return Err(ArchiveError::EntryInProgress);
        }
        Ok(vec![0; BLOCK * 2])
    }
}

/// 一個項目的檔頭：需要時先放 pax 延伸檔頭，再放 ustar 檔頭
fn entry_header(name: &str, mtime: i64, size: u64, kind: u8, mode: u64) -> Vec<u8> {
    let mut records = Vec::new();
    // 長路徑與非 ASCII 的名稱：ustar 欄位放不下，或無法保證以 UTF-8 解讀
    if name.len() > NAME_LEN || !name.is_ascii() {
        pax_record(&mut records, "path", name);
    }
    if size > OCTAL11_MAX {
        pax_record(&mut records, "size", &size.to_string());
    }
    let ustar_mtime = match u64::try_from(mtime) {
        Ok(m) if m <= OCTAL11_MAX => m,
        _ => {
            pax_record(&mut records, "mtime", &mtime.to_string());
            0
        }
    };

    let mut out = Vec::with_capacity(BLOCK * 3);
    if !records.is_empty() {
        let len = records.len() as u64;
        out.extend(ustar_header(PAX_NAME, 0, len, TYPE_PAX, FILE_MODE));
        out.extend_from_slice(&records);
        out.resize(out.len() + padding(len), 0);
    }
    // ustar 欄位放不下的值已經在 pax 中：這裡放截短或歸零的值，給不懂 pax 的程式參考
    out.extend(ustar_header(
        &truncate(name, NAME_LEN),
        ustar_mtime,
        size.min(OCTAL11_MAX),
        kind,
        mode,
    ));
    out
}

/// 一個 512 位元組的 ustar 檔頭
fn ustar_header(name: &str, mtime: u64, size: u64, kind: u8, mode: u64) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    put_bytes(&mut h[0..100], name.as_bytes());
    put_octal(&mut h[100..108], mode);
    put_octal(&mut h[108..116], 0); // uid
    put_octal(&mut h[116..124], 0); // gid
    put_octal(&mut h[124..136], size);
    put_octal(&mut h[136..148], mtime);
    h[156] = kind;
    put_bytes(&mut h[257..263], b"ustar\0");
    put_bytes(&mut h[263..265], b"00");

    // checksum：先把欄位當成 8 個空白計算，再寫成 6 位八進位數 + NUL + 空白
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    put_bytes(&mut h[148..156], format!("{sum:06o}\0 ").as_bytes());
    h
}

/// pax 紀錄："<長度> <名稱>=<值>\n"，長度是整筆紀錄的位元組數（包含長度本身的位數）
fn pax_record(out: &mut Vec<u8>, key: &str, value: &str) {
    // 空白、=、換行共 3 個位元組
    let rest = key.len() + value.len() + 3;
    let mut len = rest + 1;
    // 長度的位數會影響長度本身：重算到位數不再改變為止（最多多算一次）
    while len != rest + len.to_string().len() {
        len = rest + len.to_string().len();
    }
    out.extend_from_slice(format!("{len} {key}={value}\n").as_bytes());
}

/// 補到 512 的倍數需要幾個零
fn padding(len: u64) -> usize {
    let block = BLOCK as u64;
    ((block - len % block) % block) as usize
}

/// 最多 `max` 個位元組，不切在 UTF-8 字元的中間
fn truncate(name: &str, max: usize) -> String {
    let mut end = name.len().min(max);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

fn put_bytes(field: &mut [u8], value: &[u8]) {
    field[..value.len()].copy_from_slice(value);
}

/// 八進位數字補零到欄位長度減一，最後一個位元組是 NUL
fn put_octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    put_bytes(field, format!("{value:0width$o}").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const MTIME: i64 = 1_790_000_000;

    /// 測試用：把整個 tar 組成一個 Vec
    fn build(files: &[(&str, &[u8])], dirs: &[&str]) -> Vec<u8> {
        let mut writer = TarWriter::new();
        let mut out = Vec::new();
        for dir in dirs {
            out.extend(writer.directory(dir, MTIME).unwrap());
        }
        for (name, data) in files {
            out.extend(writer.start_file(name, MTIME, data.len() as u64).unwrap());
            writer.update(data);
            out.extend_from_slice(data);
            out.extend(writer.finish_file().unwrap());
        }
        out.extend(writer.finish().unwrap());
        out
    }

    #[test]
    fn output_is_whole_blocks() {
        let tar = build(&[("a.txt", b"hello"), ("empty", b"")], &["docs"]);
        assert_eq!(tar.len() % BLOCK, 0);
        // 資料夾、a.txt 的檔頭與內容、empty 的檔頭、結尾兩個區塊
        assert_eq!(tar.len(), BLOCK * 6);
        assert!(tar[tar.len() - BLOCK * 2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn header_checksum_is_valid() {
        let tar = build(&[("a.txt", b"hello")], &[]);
        let header = &tar[..BLOCK];
        let stored =
            u64::from_str_radix(std::str::from_utf8(&header[148..154]).unwrap(), 8).unwrap();
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(b)
                }
            })
            .sum();
        assert_eq!(stored, sum);
        assert_eq!(&header[257..263], b"ustar\0");
    }

    #[test]
    fn short_ascii_names_need_no_pax() {
        let tar = build(&[("a.txt", b"x")], &[]);
        assert_eq!(tar[156], TYPE_FILE);
    }

    #[test]
    fn pax_record_length_counts_its_own_digits() {
        // 長度從 9 變成 10 時，位數改變：最容易算錯的地方
        for value_len in 0..200 {
            let mut out = Vec::new();
            pax_record(&mut out, "path", &"a".repeat(value_len));
            let text = std::str::from_utf8(&out).unwrap();
            let (len, _) = text.split_once(' ').unwrap();
            assert_eq!(len.parse::<usize>().unwrap(), out.len(), "{text}");
        }
    }

    #[test]
    fn size_mismatch_is_an_error() {
        let mut writer = TarWriter::new();
        writer.start_file("a", MTIME, 5).unwrap();
        writer.update(b"abc");
        assert_eq!(
            writer.finish_file(),
            Err(ArchiveError::SizeMismatch {
                expected: 5,
                actual: 3
            })
        );
    }

    #[test]
    fn calls_out_of_order_are_errors() {
        let mut writer = TarWriter::new();
        assert_eq!(writer.finish_file(), Err(ArchiveError::NoEntry));
        writer.start_file("a", MTIME, 0).unwrap();
        assert_eq!(
            writer.start_file("b", MTIME, 0),
            Err(ArchiveError::EntryInProgress)
        );
        assert_eq!(
            writer.directory("c", MTIME),
            Err(ArchiveError::EntryInProgress)
        );
    }

    #[test]
    fn huge_size_and_old_mtime_go_into_pax() {
        let size = OCTAL11_MAX + 1;
        let header = entry_header("big.iso", -1, size, TYPE_FILE, FILE_MODE);
        assert_eq!(header[156], TYPE_PAX);
        let records = std::str::from_utf8(&header[BLOCK..BLOCK * 2]).unwrap();
        assert!(records.contains(&format!(" size={size}\n")), "{records}");
        assert!(records.contains(" mtime=-1\n"), "{records}");
    }

    /// 用系統的 tar 實際解開：名稱、內容、資料夾都要和原本一樣
    #[test]
    fn system_tar_extracts_it() {
        let long = format!("{}/{}.txt", "長資料夾名稱".repeat(8), "x".repeat(150));
        let files: [(&str, &[u8]); 4] = [
            ("docs/a.txt", b"hello"),
            ("相片/2026 夏天.jpg", &[7u8; 1500]),
            (&long, b"long name"),
            ("docs/empty", b""),
        ];
        let tar = build(
            &files,
            &[
                "docs",
                "相片",
                "空的資料夾",
                &long[..long.rfind('/').unwrap()],
            ],
        );

        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("test.tar");
        std::fs::write(&archive, &tar).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let status = Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());

        for (name, data) in files {
            assert_eq!(std::fs::read(out.join(name)).unwrap(), data, "{name}");
        }
        assert!(out.join("空的資料夾").is_dir());
        // pax 檔頭不能被解成檔案
        assert!(!out.join("@PaxHeader").exists());
        let modified = std::fs::metadata(out.join("docs/a.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            MTIME as u64
        );
    }
}
