//! HTTP Range 標頭（下載與影片播放的斷點續傳）：任何輸入都不能 panic，
//! 回傳的範圍一定在檔案之內，而且至少有一個位元組。
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/web/range.rs"]
#[allow(dead_code)]
mod range;

use range::RangeRequest;

fuzz_target!(|input: (&str, Option<&str>, &str, u64)| {
    let (header, if_range, etag, total) = input;

    for result in [
        range::evaluate(Some(header), if_range, etag, total),
        range::parse_range(header, total),
    ] {
        match result {
            RangeRequest::Partial(r) => {
                assert!(r.byte_count() >= 1, "empty range for {header:?}");
                let end = r.start().checked_add(r.byte_count());
                assert!(
                    end.is_some_and(|end| end <= total),
                    "{r:?} is outside a file of {total} bytes for {header:?}"
                );
                // 產生回應標頭時也不會 panic
                let _ = r.content_range(total);
            }
            RangeRequest::Full | RangeRequest::Unsatisfiable => {}
        }
    }
});
