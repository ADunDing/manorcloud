//! 串流壓縮：打包器產生的位元組送出之前，在這裡壓縮。
//!
//! 每次放進一段資料，就取回目前已經壓好的部分（可能是空的：壓縮器會先累積到一定的量）。
//! 記憶體用量固定，不隨下載的大小成長。
//!
//! 壓縮等級刻意選低的：樹莓派的 CPU 是瓶頸，等級高一點壓縮檔只小一點點，
//! 速度卻慢很多。照片、影片等本來就壓縮過的檔案，不論哪個等級都幾乎不會變小。

use std::io::{self, Write};

use flate2::Compression;
use flate2::write::GzEncoder;

/// gzip 的等級（1–9）：1 最快
const GZIP_LEVEL: u32 = 1;
/// zstd 的等級（1–22）：3 是 zstd 自己的預設值，速度仍比 gzip 的最低等級快
const ZSTD_LEVEL: i32 = 3;

pub enum Compressor {
    /// 不壓縮：資料原樣送出
    None,
    Gzip(GzEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl Compressor {
    pub fn gzip() -> Self {
        Compressor::Gzip(GzEncoder::new(Vec::new(), Compression::new(GZIP_LEVEL)))
    }

    pub fn zstd() -> io::Result<Self> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
        // 在檔頭記錄原始大小與加上校驗碼：解壓時能發現資料損毀
        encoder.include_checksum(true)?;
        Ok(Compressor::Zstd(encoder))
    }

    /// 放進一段資料，回傳目前可以送出的位元組
    pub fn write(&mut self, data: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Compressor::None => Ok(data.to_vec()),
            Compressor::Gzip(encoder) => {
                encoder.write_all(data)?;
                Ok(std::mem::take(encoder.get_mut()))
            }
            Compressor::Zstd(encoder) => {
                encoder.write_all(data)?;
                Ok(std::mem::take(encoder.get_mut()))
            }
        }
    }

    /// 結束壓縮，回傳最後剩下的位元組（包括格式的結尾）
    pub fn finish(self) -> io::Result<Vec<u8>> {
        match self {
            Compressor::None => Ok(Vec::new()),
            Compressor::Gzip(encoder) => encoder.finish(),
            Compressor::Zstd(encoder) => encoder.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// 分成很多小段放進去，再把所有輸出接起來
    fn compress(mut compressor: Compressor, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in data.chunks(1000) {
            out.extend(compressor.write(chunk).unwrap());
        }
        out.extend(compressor.finish().unwrap());
        out
    }

    fn sample() -> Vec<u8> {
        (0..300_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect()
    }

    #[test]
    fn none_passes_data_through() {
        let data = sample();
        assert_eq!(compress(Compressor::None, &data), data);
    }

    #[test]
    fn gzip_round_trips() {
        let data = sample();
        let packed = compress(Compressor::gzip(), &data);
        assert!(packed.len() < data.len() / 4);
        let mut unpacked = Vec::new();
        flate2::read::GzDecoder::new(&packed[..])
            .read_to_end(&mut unpacked)
            .unwrap();
        assert_eq!(unpacked, data);
    }

    #[test]
    fn zstd_round_trips() {
        let data = sample();
        let packed = compress(Compressor::zstd().unwrap(), &data);
        assert!(packed.len() < data.len() / 4);
        assert_eq!(zstd::decode_all(&packed[..]).unwrap(), data);
    }

    #[test]
    fn empty_input_is_still_a_valid_stream() {
        let gz = compress(Compressor::gzip(), b"");
        let mut unpacked = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut unpacked)
            .unwrap();
        assert!(unpacked.is_empty());
        let zst = compress(Compressor::zstd().unwrap(), b"");
        assert!(zstd::decode_all(&zst[..]).unwrap().is_empty());
    }
}
