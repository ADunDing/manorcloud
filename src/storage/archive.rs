//! 串流 ZIP 寫入器：一邊讀檔案一邊產生 .zip，不需要先在磁碟上建立整個壓縮檔。
//!
//! 只做「打包」不壓縮（stored）：照片、影片、壓縮檔本來就壓縮過，再壓幾乎不會變小，
//! 只會讓樹莓派的 CPU 變成瓶頸；不壓縮時下載速度只受網路與磁碟限制。
//!
//! 格式的重點（PKWARE APPNOTE 6.3）：
//! - 每個檔案：本地檔頭 → 內容 → 資料描述（CRC-32 要讀完內容才知道，所以放在內容之後，flag bit 3）
//! - 結尾：中央目錄（每個項目一筆）→ ZIP64 結尾紀錄與定位器（需要時）→ 結尾紀錄
//! - 名稱一律 UTF-8（flag bit 11），中文檔名在各平台都能正確解開
//! - 4 GiB 以上的檔案、超過 65535 個項目、或壓縮檔本身超過 4 GiB 時使用 ZIP64
//! - 修改時間：DOS 格式（2 秒精度、沒有時區）之外，再加上 Unix 時間戳記的延伸欄位
//!
//! 這個模組只負責產生位元組，不碰磁碟也不碰網路：呼叫端把每一段結果依序送出即可，
//! 也因此可以完整地在單元測試中驗證。

use std::fmt;

/// 需要改用 ZIP64 的界線：32 位元欄位的最大值保留給「請看 ZIP64 欄位」
const U32_LIMIT: u64 = 0xFFFF_FFFF;
const U16_LIMIT: u64 = 0xFFFF;

const LOCAL_HEADER_SIG: u32 = 0x0403_4b50;
const DATA_DESCRIPTOR_SIG: u32 = 0x0807_4b50;
const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
const ZIP64_END_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const END_SIG: u32 = 0x0605_4b50;

/// 解開需要的版本：2.0（一般）、4.5（ZIP64）
const VERSION_DEFAULT: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// 建立者：Unix（高位元組 3）、規格版本 4.5。外部屬性因此以 Unix 權限解讀
const VERSION_MADE_BY: u16 = (3 << 8) | VERSION_ZIP64;

/// bit 3：CRC 與大小寫在內容之後的資料描述中；bit 11：名稱是 UTF-8
const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
const FLAG_UTF8: u16 = 1 << 11;

const ZIP64_EXTRA_ID: u16 = 0x0001;
const TIMESTAMP_EXTRA_ID: u16 = 0x5455;

/// Unix 權限放在外部屬性的高 16 位元；資料夾另外設定 MS-DOS 的目錄屬性（0x10）
const FILE_ATTRIBUTES: u32 = 0o100_644 << 16;
const DIRECTORY_ATTRIBUTES: u32 = (0o040_755 << 16) | 0x10;

/// 名稱的長度欄位是 16 位元
const MAX_NAME_BYTES: usize = 0xFFFF;

#[derive(Debug, PartialEq, Eq)]
pub enum ArchiveError {
    /// 路徑太長，放不進 ZIP 的名稱欄位
    NameTooLong,
    /// 開始新項目時，上一個檔案還沒結束（呼叫順序的 bug）
    EntryInProgress,
    /// 結束檔案時，沒有正在寫入的檔案（呼叫順序的 bug）
    NoEntry,
    /// 實際讀到的大小與開始時宣告的不同：檔案在打包途中被改動了
    SizeMismatch { expected: u64, actual: u64 },
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchiveError::NameTooLong => f.write_str("path too long for a zip entry"),
            ArchiveError::EntryInProgress => f.write_str("the previous zip entry is not finished"),
            ArchiveError::NoEntry => f.write_str("no zip entry in progress"),
            ArchiveError::SizeMismatch { expected, actual } => {
                write!(
                    f,
                    "file size changed while archiving: expected {expected}, read {actual}"
                )
            }
        }
    }
}

impl std::error::Error for ArchiveError {}

/// 寫到一半的檔案
struct Pending {
    name: Vec<u8>,
    mtime: i64,
    offset: u64,
    expected: u64,
    zip64: bool,
    crc: Crc32,
    written: u64,
}

/// 串流 ZIP 寫入器。依序呼叫：
/// `directory` 或 `start_file` → `update`（多次）→ `finish_file`，最後 `finish`。
/// 每個方法回傳的位元組要依序送出；`update` 的資料由呼叫端自己送出（不複製）。
pub struct ZipWriter {
    central: Vec<u8>,
    entries: u64,
    /// 目前為止送出的位元組數：下一個本地檔頭的位置
    offset: u64,
    pending: Option<Pending>,
    /// 測試用：小檔案也使用 ZIP64，驗證 ZIP64 的格式而不必產生 4 GiB 的資料
    force_zip64: bool,
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipWriter {
    pub fn new() -> Self {
        ZipWriter {
            central: Vec::new(),
            entries: 0,
            offset: 0,
            pending: None,
            force_zip64: false,
        }
    }

    #[cfg(test)]
    fn forcing_zip64() -> Self {
        ZipWriter {
            force_zip64: true,
            ..Self::new()
        }
    }

    /// 加入一個資料夾（名稱以 / 結尾）。空資料夾也因此會出現在解開的結果中
    pub fn directory(&mut self, path: &str, mtime: i64) -> Result<Vec<u8>, ArchiveError> {
        if self.pending.is_some() {
            return Err(ArchiveError::EntryInProgress);
        }
        let mut name = path.trim_end_matches('/').as_bytes().to_vec();
        name.push(b'/');
        check_name(&name)?;

        let offset = self.offset;
        let (time, date) = dos_time(mtime);
        let mut header = Vec::with_capacity(30 + name.len() + 9);
        put_u32(&mut header, LOCAL_HEADER_SIG);
        put_u16(&mut header, VERSION_DEFAULT);
        put_u16(&mut header, FLAG_UTF8);
        put_u16(&mut header, 0); // 不壓縮
        put_u16(&mut header, time);
        put_u16(&mut header, date);
        put_u32(&mut header, 0); // CRC
        put_u32(&mut header, 0); // 壓縮後大小
        put_u32(&mut header, 0); // 原始大小
        put_u16(&mut header, name_len(&name));
        let extra = timestamp_extra(mtime);
        put_u16(&mut header, extra_len(&extra));
        header.extend_from_slice(&name);
        header.extend_from_slice(&extra);

        self.add_central(&CentralEntry {
            name: &name,
            mtime,
            flags: FLAG_UTF8,
            crc: 0,
            size: 0,
            offset,
            attributes: DIRECTORY_ATTRIBUTES,
            zip64_sizes: false,
        });
        self.offset += header.len() as u64;
        Ok(header)
    }

    /// 開始一個檔案，回傳本地檔頭。`size` 是預期的大小：決定是否需要 ZIP64，並在結束時核對
    pub fn start_file(
        &mut self,
        path: &str,
        mtime: i64,
        size: u64,
    ) -> Result<Vec<u8>, ArchiveError> {
        if self.pending.is_some() {
            return Err(ArchiveError::EntryInProgress);
        }
        let name = path.as_bytes().to_vec();
        check_name(&name)?;
        let zip64 = self.force_zip64 || size >= U32_LIMIT;

        let (time, date) = dos_time(mtime);
        let mut extra = Vec::new();
        if zip64 {
            // 本地檔頭的 ZIP64 欄位：大小此時還是 0，真正的值在資料描述中（8 位元組版本）。
            // 規格：本地檔頭有 ZIP64 欄位時，資料描述的大小必須是 8 位元組
            put_u16(&mut extra, ZIP64_EXTRA_ID);
            put_u16(&mut extra, 16);
            put_u64(&mut extra, 0);
            put_u64(&mut extra, 0);
        }
        extra.extend_from_slice(&timestamp_extra(mtime));

        let mut header = Vec::with_capacity(30 + name.len() + extra.len());
        put_u32(&mut header, LOCAL_HEADER_SIG);
        put_u16(
            &mut header,
            if zip64 {
                VERSION_ZIP64
            } else {
                VERSION_DEFAULT
            },
        );
        put_u16(&mut header, FLAG_UTF8 | FLAG_DATA_DESCRIPTOR);
        put_u16(&mut header, 0); // 不壓縮
        put_u16(&mut header, time);
        put_u16(&mut header, date);
        put_u32(&mut header, 0); // CRC：在資料描述中
        let placeholder = if zip64 { 0xFFFF_FFFF } else { 0 };
        put_u32(&mut header, placeholder);
        put_u32(&mut header, placeholder);
        put_u16(&mut header, name_len(&name));
        put_u16(&mut header, extra_len(&extra));
        header.extend_from_slice(&name);
        header.extend_from_slice(&extra);

        self.pending = Some(Pending {
            name,
            mtime,
            offset: self.offset,
            expected: size,
            zip64,
            crc: Crc32::new(),
            written: 0,
        });
        self.offset += header.len() as u64;
        Ok(header)
    }

    /// 檔案的一段內容經過這裡：累計 CRC 與長度。內容本身由呼叫端送出
    pub fn update(&mut self, data: &[u8]) {
        if let Some(pending) = self.pending.as_mut() {
            pending.crc.update(data);
            pending.written += data.len() as u64;
            self.offset += data.len() as u64;
        }
    }

    /// 結束目前的檔案，回傳資料描述（CRC 與大小）
    pub fn finish_file(&mut self) -> Result<Vec<u8>, ArchiveError> {
        let pending = self.pending.take().ok_or(ArchiveError::NoEntry)?;
        if pending.written != pending.expected {
            return Err(ArchiveError::SizeMismatch {
                expected: pending.expected,
                actual: pending.written,
            });
        }
        let crc = pending.crc.finish();

        let mut descriptor = Vec::with_capacity(24);
        put_u32(&mut descriptor, DATA_DESCRIPTOR_SIG);
        put_u32(&mut descriptor, crc);
        if pending.zip64 {
            put_u64(&mut descriptor, pending.written);
            put_u64(&mut descriptor, pending.written);
        } else {
            put_u32(&mut descriptor, low32(pending.written));
            put_u32(&mut descriptor, low32(pending.written));
        }

        self.add_central(&CentralEntry {
            name: &pending.name,
            mtime: pending.mtime,
            flags: FLAG_UTF8 | FLAG_DATA_DESCRIPTOR,
            crc,
            size: pending.written,
            offset: pending.offset,
            attributes: FILE_ATTRIBUTES,
            zip64_sizes: pending.zip64,
        });
        self.offset += descriptor.len() as u64;
        Ok(descriptor)
    }

    /// 結束整個壓縮檔：中央目錄與結尾紀錄
    pub fn finish(mut self) -> Result<Vec<u8>, ArchiveError> {
        if self.pending.is_some() {
            return Err(ArchiveError::EntryInProgress);
        }
        let central_offset = self.offset;
        let central_size = self.central.len() as u64;
        let mut tail = std::mem::take(&mut self.central);

        let needs_zip64 = self.force_zip64
            || self.entries >= U16_LIMIT
            || central_size >= U32_LIMIT
            || central_offset >= U32_LIMIT;

        if needs_zip64 {
            let zip64_end_offset = central_offset + central_size;
            put_u32(&mut tail, ZIP64_END_SIG);
            put_u64(&mut tail, 44); // 這筆紀錄其餘部分的長度
            put_u16(&mut tail, VERSION_MADE_BY);
            put_u16(&mut tail, VERSION_ZIP64);
            put_u32(&mut tail, 0); // 這個磁碟的編號
            put_u32(&mut tail, 0); // 中央目錄所在的磁碟
            put_u64(&mut tail, self.entries);
            put_u64(&mut tail, self.entries);
            put_u64(&mut tail, central_size);
            put_u64(&mut tail, central_offset);

            put_u32(&mut tail, ZIP64_LOCATOR_SIG);
            put_u32(&mut tail, 0);
            put_u64(&mut tail, zip64_end_offset);
            put_u32(&mut tail, 1); // 磁碟總數
        }

        let entries16 = if self.entries >= U16_LIMIT {
            0xFFFF
        } else {
            low16(self.entries)
        };
        put_u32(&mut tail, END_SIG);
        put_u16(&mut tail, 0);
        put_u16(&mut tail, 0);
        put_u16(&mut tail, entries16);
        put_u16(&mut tail, entries16);
        put_u32(&mut tail, clamp32(central_size));
        put_u32(&mut tail, clamp32(central_offset));
        put_u16(&mut tail, 0); // 註解長度
        Ok(tail)
    }

    fn add_central(&mut self, entry: &CentralEntry<'_>) {
        let offset_overflow = self.force_zip64 || entry.offset >= U32_LIMIT;
        let sizes_overflow = entry.zip64_sizes;

        // ZIP64 欄位只放「32 位元欄位寫成 0xFFFFFFFF」的那些值，順序固定：原始大小、壓縮後大小、位置
        let mut zip64 = Vec::new();
        if sizes_overflow {
            put_u64(&mut zip64, entry.size);
            put_u64(&mut zip64, entry.size);
        }
        if offset_overflow {
            put_u64(&mut zip64, entry.offset);
        }
        let mut extra = Vec::new();
        if !zip64.is_empty() {
            put_u16(&mut extra, ZIP64_EXTRA_ID);
            put_u16(&mut extra, extra_len(&zip64));
            extra.extend_from_slice(&zip64);
        }
        extra.extend_from_slice(&timestamp_extra(entry.mtime));

        let needs_zip64 = sizes_overflow || offset_overflow;
        let (time, date) = dos_time(entry.mtime);
        let c = &mut self.central;
        put_u32(c, CENTRAL_HEADER_SIG);
        put_u16(c, VERSION_MADE_BY);
        put_u16(
            c,
            if needs_zip64 {
                VERSION_ZIP64
            } else {
                VERSION_DEFAULT
            },
        );
        put_u16(c, entry.flags);
        put_u16(c, 0);
        put_u16(c, time);
        put_u16(c, date);
        put_u32(c, entry.crc);
        let size32 = if sizes_overflow {
            0xFFFF_FFFF
        } else {
            low32(entry.size)
        };
        put_u32(c, size32);
        put_u32(c, size32);
        put_u16(c, name_len(entry.name));
        put_u16(c, extra_len(&extra));
        put_u16(c, 0); // 註解長度
        put_u16(c, 0); // 起始磁碟
        put_u16(c, 0); // 內部屬性
        put_u32(c, entry.attributes);
        put_u32(
            c,
            if offset_overflow {
                0xFFFF_FFFF
            } else {
                low32(entry.offset)
            },
        );
        c.extend_from_slice(entry.name);
        c.extend_from_slice(&extra);
        self.entries += 1;
    }
}

struct CentralEntry<'a> {
    name: &'a [u8],
    mtime: i64,
    flags: u16,
    crc: u32,
    size: u64,
    offset: u64,
    attributes: u32,
    zip64_sizes: bool,
}

// ==================== 格式的細節 ====================

fn check_name(name: &[u8]) -> Result<(), ArchiveError> {
    if name.len() > MAX_NAME_BYTES {
        Err(ArchiveError::NameTooLong)
    } else {
        Ok(())
    }
}

/// 已由 check_name 保證不超過 16 位元
fn name_len(name: &[u8]) -> u16 {
    u16::try_from(name.len()).unwrap_or(u16::MAX)
}

/// 延伸欄位都很短（最多數十位元組）
fn extra_len(extra: &[u8]) -> u16 {
    u16::try_from(extra.len()).unwrap_or(u16::MAX)
}

/// Unix 時間戳記延伸欄位（0x5455）：只放修改時間。解壓工具用它還原精確的時間，不受時區影響
fn timestamp_extra(mtime: i64) -> Vec<u8> {
    let mut extra = Vec::with_capacity(9);
    put_u16(&mut extra, TIMESTAMP_EXTRA_ID);
    put_u16(&mut extra, 5);
    extra.push(1); // 只有修改時間
    let seconds = u32::try_from(mtime.clamp(0, i64::from(u32::MAX))).unwrap_or(0);
    put_u32(&mut extra, seconds);
    extra
}

/// DOS 格式的時間與日期（UTC，2 秒精度）。1980 年以前的時間無法表示，一律視為 1980-01-01
fn dos_time(mtime: i64) -> (u16, u16) {
    let Ok(datetime) = time::OffsetDateTime::from_unix_timestamp(mtime) else {
        return (0, (1 << 5) | 1);
    };
    let year = datetime.year();
    if !(1980..=2107).contains(&year) {
        return (0, (1 << 5) | 1);
    }
    let time = (u16::from(datetime.hour()) << 11)
        | (u16::from(datetime.minute()) << 5)
        | (u16::from(datetime.second()) / 2);
    let date = (u16::try_from(year - 1980).unwrap_or(0) << 9)
        | (u16::from(u8::from(datetime.month())) << 5)
        | u16::from(datetime.day());
    (time, date)
}

fn low32(value: u64) -> u32 {
    u32::try_from(value & U32_LIMIT).unwrap_or(u32::MAX)
}

fn low16(value: u64) -> u16 {
    u16::try_from(value & U16_LIMIT).unwrap_or(u16::MAX)
}

fn clamp32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn put_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

// ==================== CRC-32 ====================

/// ZIP 使用的 CRC-32（IEEE 802.3，反射多項式 0xEDB88320），查表法
struct Crc32 {
    value: u32,
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

impl Crc32 {
    fn new() -> Self {
        Crc32 { value: 0xFFFF_FFFF }
    }

    fn update(&mut self, data: &[u8]) {
        let mut c = self.value;
        for &byte in data {
            c = CRC_TABLE[usize::from((c as u8) ^ byte)] ^ (c >> 8);
        }
        self.value = c;
    }

    fn finish(&self) -> u32 {
        self.value ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 測試用：把整個壓縮檔組成一個 Vec
    fn build(writer: &mut ZipWriter, files: &[(&str, &[u8])], dirs: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for dir in dirs {
            out.extend(writer.directory(dir, 1_790_000_000).unwrap());
        }
        for (name, data) in files {
            out.extend(
                writer
                    .start_file(name, 1_790_000_000, data.len() as u64)
                    .unwrap(),
            );
            // 分成小段送入，模擬串流
            for chunk in data.chunks(7) {
                writer.update(chunk);
                out.extend_from_slice(chunk);
            }
            out.extend(writer.finish_file().unwrap());
        }
        out
    }

    fn u16_at(buf: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([buf[at], buf[at + 1]])
    }

    fn u32_at(buf: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
    }

    fn u64_at(buf: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
    }

    /// 最小的讀取器：從結尾找到中央目錄，取出每個項目的名稱、大小、CRC，並用 CRC 驗證內容。
    /// 不依賴其他套件，與寫入器互相獨立地檢查格式
    fn read_back(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let end = zip.len() - 22;
        assert_eq!(u32_at(zip, end), END_SIG);
        let mut count = u64::from(u16_at(zip, end + 10));
        let mut cd_offset = u64::from(u32_at(zip, end + 16));
        if count == 0xFFFF || cd_offset == 0xFFFF_FFFF {
            let locator = end - 20;
            assert_eq!(u32_at(zip, locator), ZIP64_LOCATOR_SIG);
            let record = usize::try_from(u64_at(zip, locator + 8)).unwrap();
            assert_eq!(u32_at(zip, record), ZIP64_END_SIG);
            count = u64_at(zip, record + 32);
            cd_offset = u64_at(zip, record + 48);
        }

        let mut at = usize::try_from(cd_offset).unwrap();
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(zip, at), CENTRAL_HEADER_SIG);
            let crc = u32_at(zip, at + 16);
            let mut size = u64::from(u32_at(zip, at + 24));
            let name_len = usize::from(u16_at(zip, at + 28));
            let extra_len = usize::from(u16_at(zip, at + 30));
            let mut offset = u64::from(u32_at(zip, at + 42));
            let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();

            // ZIP64 欄位
            let mut e = at + 46 + name_len;
            let extra_end = e + extra_len;
            while e < extra_end {
                let id = u16_at(zip, e);
                let len = usize::from(u16_at(zip, e + 2));
                if id == ZIP64_EXTRA_ID {
                    let mut f = e + 4;
                    if size == 0xFFFF_FFFF {
                        size = u64_at(zip, f);
                        f += 16;
                    }
                    if offset == 0xFFFF_FFFF {
                        offset = u64_at(zip, f);
                    }
                }
                e += 4 + len;
            }

            let local = usize::try_from(offset).unwrap();
            assert_eq!(u32_at(zip, local), LOCAL_HEADER_SIG);
            let data_start = local
                + 30
                + usize::from(u16_at(zip, local + 26))
                + usize::from(u16_at(zip, local + 28));
            let data = zip[data_start..data_start + usize::try_from(size).unwrap()].to_vec();
            let mut check = Crc32::new();
            check.update(&data);
            assert_eq!(check.finish(), crc, "{name}");

            out.push((name, data));
            at = extra_end;
        }
        out
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }

    #[test]
    fn files_and_folders_round_trip() {
        let mut writer = ZipWriter::new();
        let mut zip = build(
            &mut writer,
            &[
                ("相片/旅行.jpg", b"photo bytes"),
                ("notes.txt", b""),
                ("相片/2026/a.bin", &[0u8, 1, 2, 255]),
            ],
            &["相片", "相片/2026", "空的"],
        );
        zip.extend(writer.finish().unwrap());

        let entries = read_back(&zip);
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "相片/",
                "相片/2026/",
                "空的/",
                "相片/旅行.jpg",
                "notes.txt",
                "相片/2026/a.bin"
            ]
        );
        assert_eq!(entries[3].1, b"photo bytes");
        assert!(entries[4].1.is_empty());
    }

    #[test]
    fn zip64_layout_round_trips() {
        let mut writer = ZipWriter::forcing_zip64();
        let mut zip = build(
            &mut writer,
            &[("big.bin", b"pretend this is 5 GiB")],
            &["dir"],
        );
        zip.extend(writer.finish().unwrap());

        let entries = read_back(&zip);
        assert_eq!(
            entries[1],
            ("big.bin".to_string(), b"pretend this is 5 GiB".to_vec())
        );
    }

    #[test]
    fn size_change_while_archiving_is_an_error() {
        let mut writer = ZipWriter::new();
        writer.start_file("a.txt", 0, 10).unwrap();
        writer.update(b"short");
        assert_eq!(
            writer.finish_file(),
            Err(ArchiveError::SizeMismatch {
                expected: 10,
                actual: 5
            })
        );
    }

    #[test]
    fn calls_out_of_order_are_errors() {
        let mut writer = ZipWriter::new();
        assert_eq!(writer.finish_file(), Err(ArchiveError::NoEntry));
        writer.start_file("a.txt", 0, 0).unwrap();
        assert_eq!(
            writer.start_file("b.txt", 0, 0).err(),
            Some(ArchiveError::EntryInProgress)
        );
        assert_eq!(
            writer.directory("d", 0).err(),
            Some(ArchiveError::EntryInProgress)
        );
    }

    #[test]
    fn times_before_1980_are_clamped() {
        assert_eq!(dos_time(0), (0, (1 << 5) | 1));
        // 2026-11-14 12:34:56 UTC
        let (time, date) = dos_time(1_794_659_696);
        assert_eq!(time >> 11, 12);
        assert_eq!((time >> 5) & 0x3F, 34);
        assert_eq!((date >> 9) + 1980, 2026);
    }
}
