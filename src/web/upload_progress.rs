//! 同時上傳多段時，記住哪些段落已經寫進硬碟，但前面還有空缺。
//!
//! 資料庫只記錄「從頭開始連續收到多少」（uploads.received）。後面的段落先傳完時，
//! 記在這裡；前面的空缺補上之後，已確認長度一次推進到最遠的地方。
//!
//! 只放在記憶體中：程式重新啟動後遺失也沒關係，前端從資料庫的已確認長度開始重送，
//! 最多多傳幾段。鎖只在讀寫的瞬間持有，從不跨過 await。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Default)]
pub struct UploadProgress(Arc<Mutex<HashMap<String, Entry>>>);

#[derive(Default)]
struct Entry {
    /// 已經落盤、但還沒被算進已確認長度的段落 [起點, 終點)
    done: Vec<(u64, u64)>,
    /// 正在寫入的段落起點：同一段被重送時，舊的請求可能還沒結束
    active: HashSet<u64>,
}

impl Entry {
    fn is_empty(&self) -> bool {
        self.done.is_empty() && self.active.is_empty()
    }
}

/// 正在寫入的一段。被丟棄時（包括請求出錯或中斷）自動登記為結束
pub struct Active {
    progress: UploadProgress,
    id: String,
    start: u64,
}

impl Drop for Active {
    fn drop(&mut self) {
        let mut map = self.progress.lock();
        if let Some(entry) = map.get_mut(&self.id) {
            entry.active.remove(&self.start);
            if entry.is_empty() {
                map.remove(&self.id);
            }
        }
    }
}

impl UploadProgress {
    /// 持有鎖的請求 panic 後仍繼續運作：這裡的資料只是最佳化，壞了最多多傳幾段
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 開始寫入從 `start` 開始的一段。同一個位置已經有請求在寫時回傳 None
    pub fn begin(&self, id: &str, start: u64) -> Option<Active> {
        let mut map = self.lock();
        let entry = map.entry(id.to_string()).or_default();
        if !entry.active.insert(start) {
            return None;
        }
        Some(Active {
            progress: self.clone(),
            id: id.to_string(),
            start,
        })
    }

    /// 記下 [start, end) 已經落盤
    pub fn record(&self, id: &str, start: u64, end: u64) {
        if end > start {
            self.lock()
                .entry(id.to_string())
                .or_default()
                .done
                .push((start, end));
        }
    }

    /// 已確認長度是 `received` 時，加上已落盤的段落，可以連續推進到哪裡
    pub fn reach(&self, id: &str, received: u64) -> u64 {
        let map = self.lock();
        let Some(entry) = map.get(id) else {
            return received;
        };
        let mut done = entry.done.clone();
        done.sort_unstable();
        let mut reach = received;
        for (start, end) in done {
            if start > reach {
                break;
            }
            reach = reach.max(end);
        }
        reach
    }

    /// 已確認長度已經推進到 `received`：在它之前結束的段落不必再記
    pub fn confirm(&self, id: &str, received: u64) {
        let mut map = self.lock();
        if let Some(entry) = map.get_mut(id) {
            entry.done.retain(|&(_, end)| end > received);
            if entry.is_empty() {
                map.remove(id);
            }
        }
    }

    /// 上傳完成或取消：全部忘掉
    pub fn forget(&self, id: &str) {
        self.lock().remove(id);
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "a";

    #[test]
    fn a_gap_holds_back_later_segments_until_it_is_filled() {
        let progress = UploadProgress::default();
        progress.record(ID, 20, 30);
        progress.record(ID, 10, 20);

        // 0..10 還沒到
        assert_eq!(progress.reach(ID, 0), 0);

        progress.record(ID, 0, 10);
        assert_eq!(progress.reach(ID, 0), 30);

        progress.confirm(ID, 30);
        assert_eq!(progress.tracked(), 0);
    }

    #[test]
    fn overlapping_and_already_confirmed_segments_are_fine() {
        let progress = UploadProgress::default();
        // 中斷的請求只寫了一部分，重送的段落與它重疊
        progress.record(ID, 10, 15);
        progress.record(ID, 10, 20);
        progress.record(ID, 0, 5);

        assert_eq!(progress.reach(ID, 10), 20);
        // 已確認長度比段落的起點更遠也沒關係
        assert_eq!(progress.reach(ID, 12), 20);
    }

    #[test]
    fn the_same_start_cannot_be_written_twice_at_once() {
        let progress = UploadProgress::default();
        let first = progress.begin(ID, 0).unwrap();

        assert!(progress.begin(ID, 0).is_none());
        // 其他位置不受影響
        let other = progress.begin(ID, 64).unwrap();

        drop(first);
        assert!(progress.begin(ID, 0).is_some());
        drop(other);
        assert_eq!(progress.tracked(), 0);
    }

    #[test]
    fn forgetting_drops_everything() {
        let progress = UploadProgress::default();
        progress.record(ID, 10, 20);

        progress.forget(ID);

        assert_eq!(progress.reach(ID, 0), 0);
        assert_eq!(progress.tracked(), 0);
    }
}
