//! HTTP Range 請求的解析（RFC 9110 §14）。
//!
//! 只支援單一範圍。原則：拿不準就回 `Full`。
//! RFC 允許伺服器完全忽略 Range，回傳整個檔案永遠是正確的回應，最壞只是多傳一些資料；
//! 錯誤的 206 卻會讓客戶端把錯誤的位元組拼進檔案，造成靜默的資料損壞。

/// 一個位元組範圍，兩端都包含。與 `Content-Range: bytes {start}-{end}/{total}` 的寫法一致。
///
/// 欄位是私有的，只能經由 `parse_range` 建立，所以型別本身保證：
/// `start <= end`，而且至少有 1 個位元組，`byte_count` 不會溢位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    pub fn start(&self) -> u64 {
        self.start
    }

    /// 這個範圍的位元組數（一定至少 1）
    pub fn byte_count(&self) -> u64 {
        self.end - self.start + 1
    }

    /// 206 回應的 Content-Range 值，例如 `bytes 0-99/1234`
    pub fn content_range(&self, total: u64) -> String {
        format!("bytes {}-{}/{total}", self.start, self.end)
    }
}

/// 416 回應的 Content-Range 值，例如 `bytes */1234`
pub fn unsatisfied_content_range(total: u64) -> String {
    format!("bytes */{total}")
}

/// Range header 的解析結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRequest {
    /// 忽略 Range，回傳整個檔案（200）
    Full,
    /// 單一有效範圍（206）
    Partial(ByteRange),
    /// 語法正確，但範圍不在檔案內（416）
    Unsatisfiable,
}

/// 結合 Range 與 If-Range，決定這次要回傳什麼。
///
/// - 沒有 Range：整個檔案（If-Range 單獨出現時忽略）
/// - 有 If-Range 但與目前的 ETag 不相符：整個檔案。
///   客戶端手上的舊資料已經沒用了，即使 Range 本身會得到 416，給它完整的新內容才是它需要的，
///   所以 If-Range 先於 Range 判斷。
/// - 其他情況：照常解析 Range
pub fn evaluate(
    range: Option<&str>,
    if_range: Option<&str>,
    etag: &str,
    total: u64,
) -> RangeRequest {
    let Some(range) = range else {
        return RangeRequest::Full;
    };
    if let Some(value) = if_range
        && !if_range_matches(value, etag)
    {
        return RangeRequest::Full;
    }
    parse_range(range, total)
}

/// If-Range 的值是否與目前的 ETag 相符。
///
/// RFC 規定 If-Range 使用強比對：必須逐字相同。因此以下情況自然都不相符：
/// - 弱 ETag（`W/"..."`）：開頭多了 `W/`，永遠不會等於我們的強 ETag
/// - 日期格式：我們不送 Last-Modified，客戶端本來就不該帶日期；
///   而且日期只精確到秒，同一秒內被替換的內容無法區分。視為不相符，最多只是多傳一次整個檔案。
pub fn if_range_matches(value: &str, etag: &str) -> bool {
    value.trim() == etag
}

/// 解析 Range header 的值。`total` 是檔案大小。
pub fn parse_range(header: &str, total: u64) -> RangeRequest {
    use RangeRequest::{Full, Partial, Unsatisfiable};

    // 空檔案無法表達任何 206（沒有「第一個」或「最後一個」位元組），一律回整個檔案
    if total == 0 {
        return Full;
    }

    // 範圍單位：只認得 bytes，比對不分大小寫；不認得的單位必須忽略
    let Some((unit, spec)) = header.trim().split_once('=') else {
        return Full;
    };
    if !unit.eq_ignore_ascii_case("bytes") {
        return Full;
    }

    // 多段範圍需要 multipart/byteranges 回應，選擇不支援；RFC 允許忽略
    if spec.contains(',') {
        return Full;
    }

    let Some((first, last)) = spec.split_once('-') else {
        return Full;
    };

    match (first.is_empty(), last.is_empty()) {
        // "bytes=-"：兩邊都沒有數字
        (true, true) => Full,

        // 後綴範圍 "bytes=-N"：最後 N 個位元組
        (true, false) => {
            let Some(suffix) = parse_position(last) else {
                return Full;
            };
            if suffix == 0 {
                // 後綴長度為 0：RFC 規定不可滿足
                return Unsatisfiable;
            }
            // 後綴比檔案長時，取整個檔案
            let len = suffix.min(total);
            Partial(ByteRange {
                start: total - len,
                end: total - 1,
            })
        }

        // "bytes=A-B" 或 "bytes=A-"
        (false, _) => {
            let Some(start) = parse_position(first) else {
                return Full;
            };
            // 沒有結尾代表「到檔案結束」
            let end = if last.is_empty() {
                u64::MAX
            } else {
                match parse_position(last) {
                    Some(end) => end,
                    None => return Full,
                }
            };

            // 結尾小於開頭：RFC 規定是「無效」而不是「無法滿足」，無效就忽略。
            // 這個判斷必須在「起點超出檔案」之前：無效的 Range 不應得到 416。
            if end < start {
                return Full;
            }
            if start >= total {
                return Unsatisfiable;
            }
            // 結尾超過檔案大小時截到最後一個位元組
            Partial(ByteRange {
                start,
                end: end.min(total - 1),
            })
        }
    }
}

/// 解析位元組位置：只接受 ASCII 數字。
///
/// - 不用 `str::parse`：它接受 "+5"，但 `+` 不在 HTTP 的語法裡。
/// - 溢位視為 `u64::MAX`（「比任何檔案都大的位置」）而不是錯誤：
///   語法上它就是一串合法的數字。當起點時一定超出檔案 → 416；
///   當終點時被截到檔案結尾；當後綴長度時代表整個檔案。三種情況都自然得到正確結果。
fn parse_position(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.bytes().fold(0u64, |acc, b| {
        acc.saturating_mul(10).saturating_add(u64::from(b - b'0'))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use RangeRequest::{Full, Partial, Unsatisfiable};

    /// 測試用檔案大小
    const TOTAL: u64 = 100;
    const ETAG: &str = "\"0123456789abcdef0123456789abcdef\"";

    fn range(start: u64, end: u64) -> RangeRequest {
        Partial(ByteRange { start, end })
    }

    fn parse(header: &str) -> RangeRequest {
        parse_range(header, TOTAL)
    }

    // ==================== 一般情況 ====================

    #[test]
    fn closed_range() {
        assert_eq!(parse("bytes=0-49"), range(0, 49));
        assert_eq!(parse("bytes=10-19"), range(10, 19));
    }

    #[test]
    fn open_ended_range() {
        assert_eq!(parse("bytes=50-"), range(50, 99));
        // 從頭到尾：仍是 206，只是範圍剛好是整個檔案
        assert_eq!(parse("bytes=0-"), range(0, 99));
    }

    #[test]
    fn suffix_range() {
        assert_eq!(parse("bytes=-10"), range(90, 99));
        assert_eq!(parse("bytes=-100"), range(0, 99));
    }

    // ==================== 指定的邊界案例 ====================

    #[test]
    fn single_first_byte() {
        assert_eq!(parse("bytes=0-0"), range(0, 0));
    }

    #[test]
    fn single_last_byte() {
        assert_eq!(parse("bytes=99-99"), range(99, 99));
        assert_eq!(parse("bytes=99-"), range(99, 99));
        assert_eq!(parse("bytes=-1"), range(99, 99));
    }

    #[test]
    fn end_beyond_total_is_clamped() {
        assert_eq!(parse("bytes=0-999999"), range(0, 99));
        assert_eq!(parse("bytes=50-100"), range(50, 99));
    }

    #[test]
    fn start_beyond_total_is_unsatisfiable() {
        assert_eq!(parse("bytes=200-"), Unsatisfiable);
        // 剛好等於檔案大小：最後一個位元組是 99，100 已經超出
        assert_eq!(parse("bytes=100-"), Unsatisfiable);
        assert_eq!(parse("bytes=100-200"), Unsatisfiable);
    }

    #[test]
    fn start_greater_than_end_is_invalid_not_unsatisfiable() {
        assert_eq!(parse("bytes=500-100"), Full);
        assert_eq!(parse("bytes=10-9"), Full);
        // 即使起點也超出檔案，無效仍優先於無法滿足
        assert_eq!(parse("bytes=300-200"), Full);
    }

    #[test]
    fn zero_suffix_is_unsatisfiable() {
        assert_eq!(parse("bytes=-0"), Unsatisfiable);
    }

    #[test]
    fn suffix_longer_than_file_is_whole_file() {
        assert_eq!(parse("bytes=-999999"), range(0, 99));
    }

    #[test]
    fn empty_file_ignores_every_range() {
        for header in ["bytes=0-0", "bytes=0-", "bytes=-5", "bytes=-0", "bytes=5-"] {
            assert_eq!(parse_range(header, 0), Full, "{header}");
        }
    }

    #[test]
    fn overflowing_numbers_are_treated_as_infinitely_large() {
        // 起點溢位：一定超出檔案
        assert_eq!(parse("bytes=99999999999999999999999-"), Unsatisfiable);
        // 終點溢位：截到檔案結尾
        assert_eq!(parse("bytes=0-99999999999999999999999"), range(0, 99));
        // 後綴溢位：整個檔案
        assert_eq!(parse("bytes=-99999999999999999999999"), range(0, 99));
    }

    #[test]
    fn unknown_unit_is_ignored() {
        assert_eq!(parse("items=0-5"), Full);
        assert_eq!(parse("bits=0-5"), Full);
    }

    #[test]
    fn unit_is_case_insensitive() {
        assert_eq!(parse("Bytes=0-5"), range(0, 5));
        assert_eq!(parse("BYTES=0-5"), range(0, 5));
    }

    #[test]
    fn multiple_ranges_are_ignored() {
        assert_eq!(parse("bytes=0-1,5-6"), Full);
        assert_eq!(parse("bytes=0-1, 5-6"), Full);
        assert_eq!(parse("bytes=0-1,"), Full);
    }

    #[test]
    fn whitespace() {
        // 整個 header 前後的空白無害（HTTP 解析時通常已經去掉）
        assert_eq!(parse("  bytes=0-5  "), range(0, 5));
        // 範圍內部的空白不在語法中
        for header in ["bytes= 0-5", "bytes=0 -5", "bytes=0- 5", "bytes =0-5"] {
            assert_eq!(parse(header), Full, "{header:?}");
        }
    }

    #[test]
    fn malformed_headers_are_ignored() {
        for header in [
            "",
            "bytes",
            "bytes=",
            "bytes=-",
            "bytes=5",
            "bytes=a-b",
            "bytes=1-2-3",
            "bytes=+5-10", // str::parse 會接受的 +
            "bytes=-+5",
            "bytes=0x10-20", // 十六進位
            "bytes=٣-٥",     // 非 ASCII 的數字
            "garbage",
        ] {
            assert_eq!(parse(header), Full, "{header:?}");
        }
    }

    #[test]
    fn leading_zeros_are_allowed() {
        assert_eq!(parse("bytes=007-010"), range(7, 10));
    }

    #[test]
    fn byte_count_includes_both_ends() {
        assert_eq!(ByteRange { start: 0, end: 0 }.byte_count(), 1);
        assert_eq!(ByteRange { start: 0, end: 99 }.byte_count(), 100);
        assert_eq!(ByteRange { start: 90, end: 99 }.byte_count(), 10);
    }

    #[test]
    fn content_range_formats() {
        assert_eq!(
            ByteRange { start: 0, end: 99 }.content_range(1234),
            "bytes 0-99/1234"
        );
        assert_eq!(unsatisfied_content_range(1234), "bytes */1234");
    }

    // ==================== If-Range ====================

    #[test]
    fn no_range_means_full_even_with_if_range() {
        assert_eq!(evaluate(None, Some(ETAG), ETAG, TOTAL), Full);
        assert_eq!(evaluate(None, None, ETAG, TOTAL), Full);
    }

    #[test]
    fn matching_if_range_allows_range() {
        assert_eq!(
            evaluate(Some("bytes=0-9"), Some(ETAG), ETAG, TOTAL),
            range(0, 9)
        );
        // 值前後的空白無害
        let padded = format!("  {ETAG}  ");
        assert_eq!(
            evaluate(Some("bytes=0-9"), Some(&padded), ETAG, TOTAL),
            range(0, 9)
        );
    }

    #[test]
    fn without_if_range_range_is_used() {
        assert_eq!(evaluate(Some("bytes=0-9"), None, ETAG, TOTAL), range(0, 9));
    }

    #[test]
    fn mismatching_if_range_means_full() {
        let other = "\"ffffffffffffffffffffffffffffffff\"";
        assert_eq!(evaluate(Some("bytes=0-9"), Some(other), ETAG, TOTAL), Full);
    }

    #[test]
    fn mismatching_if_range_beats_unsatisfiable_range() {
        // Range 本身會得到 416，但內容已經換了，給完整的新檔案
        let other = "\"ffffffffffffffffffffffffffffffff\"";
        assert_eq!(evaluate(Some("bytes=500-"), Some(other), ETAG, TOTAL), Full);
    }

    #[test]
    fn weak_etag_never_matches() {
        let weak = format!("W/{ETAG}");
        assert!(!if_range_matches(&weak, ETAG));
        assert_eq!(evaluate(Some("bytes=0-9"), Some(&weak), ETAG, TOTAL), Full);
    }

    #[test]
    fn date_if_range_never_matches() {
        let date = "Wed, 21 Oct 2026 07:28:00 GMT";
        assert!(!if_range_matches(date, ETAG));
        assert_eq!(evaluate(Some("bytes=0-9"), Some(date), ETAG, TOTAL), Full);
    }

    #[test]
    fn unquoted_etag_does_not_match() {
        // ETag 的引號是值的一部分，少了引號就不是同一個值
        let unquoted = ETAG.trim_matches('"');
        assert!(!if_range_matches(unquoted, ETAG));
    }
}
