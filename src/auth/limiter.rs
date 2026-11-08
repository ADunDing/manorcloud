//! 登入次數限制：同一個 IP 或同一個帳號失敗太多次，就暫時拒絕登入。
//!
//! 兩種計數各防一種攻擊，門檻不同：
//! - 每個 IP（5 次）：一台裝置亂猜。門檻低，單一裝置很快被擋下
//! - 每個帳號（30 次）：多台裝置合力猜同一個帳號。門檻高，
//!   讓別人很難只靠猜錯密碼，就把真正的使用者鎖在外面
//!
//! 「進行中」的嘗試也計入上限：Argon2 驗證需要一段時間，若只計算已經結束的失敗，
//! 同時送出的大量請求會在第一個失敗被記錄之前全部通過檢查。
//! 開始時用 `begin` 佔用名額，結束時用 `end` 歸還並記錄結果。
//!
//! 記在記憶體，不記在資料庫：每次失敗不寫磁碟（否則攻擊者可以藉此磨損 eMMC），
//! 重啟後歸零也不是漏洞（能重啟服務的人本來就有 root 權限）。
//!
//! 時間一律由呼叫端傳入，而且是單調時鐘（Instant）：
//! 沒有硬體時鐘的機器開機時系統時間可能跳動，單調時鐘不受影響；測試也不必真的等待。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant};

/// 同一個 IP 在計數視窗內失敗幾次就鎖定
pub const MAX_IP_FAILURES: u32 = 5;
/// 同一個帳號在計數視窗內失敗幾次就鎖定
pub const MAX_ACCOUNT_FAILURES: u32 = 30;
/// 計數視窗：從第一次失敗起算
pub const WINDOW: Duration = Duration::from_secs(15 * 60);
/// 鎖定多久
pub const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// 名額被進行中的嘗試佔滿、但還沒鎖定時，請用戶端多久後再試：
/// 屆時那些驗證已經結束，結果不是鎖定、就是名額空出來了
pub const BUSY_RETRY: Duration = Duration::from_secs(2);

/// 最多記錄幾筆：攻擊者不能用大量假帳號名稱塞滿記憶體
const MAX_TRACKED: usize = 10_000;
/// 帳號名稱只取前幾個字元當作計數的鍵：超長的名稱不會佔用更多記憶體
const MAX_KEY_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Ip(IpAddr),
    User(String),
}

impl Key {
    fn max_failures(&self) -> u32 {
        match self {
            Key::Ip(_) => MAX_IP_FAILURES,
            Key::User(_) => MAX_ACCOUNT_FAILURES,
        }
    }
}

/// 一次嘗試的結果
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// 密碼錯誤（或帳號不存在）
    Failure,
    /// 登入成功
    Success,
    /// 密碼正確但沒有登入（例如帳號已停用）：不算猜錯，也不清除計數
    Neither,
}

#[derive(Debug)]
struct Entry {
    failures: u32,
    /// 已經開始、還沒結束的嘗試
    in_flight: u32,
    /// 這個計數視窗從何時開始（第一次失敗的時刻）
    window_start: Instant,
    /// 鎖定到何時；None 代表沒有被鎖定
    locked_until: Option<Instant>,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Entry {
            failures: 0,
            in_flight: 0,
            window_start: now,
            locked_until: None,
        }
    }

    /// 還要鎖多久；沒有被鎖定或已經過期時回傳 None
    fn remaining_lock(&self, now: Instant) -> Option<Duration> {
        self.locked_until
            .filter(|until| *until > now)
            .map(|until| until - now)
    }

    /// 鎖定已經結束、或計數視窗已經過了：重新開始計數。進行中的嘗試保留
    fn refresh(&mut self, now: Instant) {
        let lock_over = self.locked_until.is_some_and(|until| until <= now);
        let window_over =
            self.locked_until.is_none() && now.duration_since(self.window_start) >= WINDOW;
        if lock_over || window_over {
            self.failures = 0;
            self.window_start = now;
            self.locked_until = None;
        }
    }

    /// 記一次失敗，回傳這次是否造成新的鎖定
    fn fail(&mut self, max: u32, now: Instant) -> bool {
        self.refresh(now);
        // 已經被同時進行的其他嘗試鎖定了：不重複計數，也不延長鎖定
        if self.remaining_lock(now).is_some() {
            return false;
        }
        // 計數視窗從第一次失敗起算，而不是從第一次嘗試
        if self.failures == 0 {
            self.window_start = now;
        }
        self.failures += 1;
        if self.failures >= max {
            self.locked_until = Some(now + LOCKOUT);
            return true;
        }
        false
    }

    /// 已經沒有任何作用的記錄：沒有進行中的嘗試、沒有被鎖定，也沒有仍在視窗內的失敗
    fn is_expired(&self, now: Instant) -> bool {
        self.in_flight == 0
            && self.remaining_lock(now).is_none()
            && (self.failures == 0 || now.duration_since(self.window_start) >= WINDOW)
    }

    /// 可以被淘汰：沒有進行中的嘗試，也沒有被鎖定
    fn evictable(&self, now: Instant) -> bool {
        self.in_flight == 0 && self.remaining_lock(now).is_none()
    }
}

/// 一次失敗後，是否有新的鎖定發生（用來記錄日誌）
#[derive(Debug, Default, PartialEq)]
pub struct NewLockouts {
    pub ip: bool,
    pub user: bool,
}

#[derive(Debug, Default)]
pub struct LoginLimiter {
    entries: HashMap<Key, Entry>,
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 嘗試開始之前呼叫：通過時為 IP 與帳號各佔用一個名額。
    ///
    /// 被拒絕時回傳還要等多久：已被鎖定時是剩餘的鎖定時間；
    /// 名額被進行中的嘗試佔滿時是 BUSY_RETRY。
    /// 通過之後，無論結果如何都必須呼叫一次 `end` 歸還名額。
    pub fn begin(&mut self, ip: IpAddr, username: &str, now: Instant) -> Result<(), Duration> {
        let keys = [ip_key(ip), user_key(username)];

        for key in &keys {
            if !self.entries.contains_key(key) {
                self.make_room(now);
            }
        }

        // 兩者都檢查完、都通過，才一起佔用名額：只佔一邊會讓計數不一致
        let mut wait: Option<Duration> = None;
        for key in &keys {
            let entry = self
                .entries
                .entry(key.clone())
                .or_insert_with(|| Entry::new(now));
            entry.refresh(now);

            let this_wait = match entry.remaining_lock(now) {
                Some(remaining) => Some(remaining),
                None if entry.failures + entry.in_flight >= key.max_failures() => Some(BUSY_RETRY),
                None => None,
            };
            // Option 的比較中 None 最小：取兩者中需要等比較久的
            wait = wait.max(this_wait);
        }

        if let Some(wait) = wait {
            return Err(wait);
        }
        for key in &keys {
            if let Some(entry) = self.entries.get_mut(key) {
                entry.in_flight += 1;
            }
        }
        Ok(())
    }

    /// 嘗試結束時呼叫：歸還 `begin` 佔用的名額，並記錄結果
    pub fn end(
        &mut self,
        ip: IpAddr,
        username: &str,
        outcome: Outcome,
        now: Instant,
    ) -> NewLockouts {
        let mut lockouts = NewLockouts::default();

        for (key, locked) in [
            (ip_key(ip), &mut lockouts.ip),
            (user_key(username), &mut lockouts.user),
        ] {
            // 進行中的記錄不會被清除或淘汰，正常情況下一定存在
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            entry.in_flight = entry.in_flight.saturating_sub(1);

            match outcome {
                Outcome::Failure => *locked = entry.fail(key.max_failures(), now),
                // 只清除帳號的計數。IP 的計數刻意保留：否則攻擊者用自己的帳號登入一次，
                // 就能把所在 IP 的計數歸零，再繼續猜別人的密碼
                Outcome::Success if matches!(key, Key::User(_)) => {
                    entry.failures = 0;
                    entry.locked_until = None;
                }
                Outcome::Success | Outcome::Neither => {}
            }
        }

        lockouts
    }

    /// 刪除已經沒有作用的記錄
    pub fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, entry| !entry.is_expired(now));
    }

    /// 記錄已滿時騰出空間：先清掉過期的，還是滿的就刪掉最舊的一筆
    fn make_room(&mut self, now: Instant) {
        if self.entries.len() < MAX_TRACKED {
            return;
        }
        self.prune(now);
        if self.entries.len() < MAX_TRACKED {
            return;
        }
        // 只淘汰「沒有進行中、也沒有鎖定」的記錄：否則被鎖住的攻擊者可以送出大量假帳號名稱，
        // 把自己那筆鎖定擠掉，再繼續猜
        let oldest = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.evictable(now))
            .min_by_key(|(_, entry)| entry.window_start)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.entries.remove(&key);
        }
    }
}

/// IP 的計數鍵。
/// - IPv4 對應的 IPv6（::ffff:1.2.3.4）視為 IPv4：同一個位址不能有兩種寫法
/// - 其他 IPv6 以 /64 為單位：一台裝置可以在自己的 /64 裡隨意更換位址
fn ip_key(ip: IpAddr) -> Key {
    let normalized = match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let prefix = u128::from(v6) & !((1u128 << 64) - 1);
                IpAddr::V6(Ipv6Addr::from(prefix))
            }
        },
    };
    Key::Ip(normalized)
}

/// 帳號的計數鍵：轉小寫（Admin 與 admin 不能各自計數），只取前 64 個字元
fn user_key(username: &str) -> Key {
    Key::User(
        username
            .trim()
            .chars()
            .take(MAX_KEY_CHARS)
            .flat_map(char::to_lowercase)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// 第 n 個 IPv4 位址（10.0.0.0 起算），需要大量不同的位址時使用
    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n))
    }

    /// 完整的一次嘗試：開始、結束
    fn attempt(
        limiter: &mut LoginLimiter,
        ip: IpAddr,
        user: &str,
        outcome: Outcome,
        now: Instant,
    ) -> NewLockouts {
        limiter.begin(ip, user, now).unwrap();
        limiter.end(ip, user, outcome, now)
    }

    // ==================== IP 的計數 ====================

    #[test]
    fn below_the_ip_limit_is_allowed() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        for n in 0..MAX_IP_FAILURES - 1 {
            attempt(
                &mut limiter,
                ip(1),
                &format!("user{n}"),
                Outcome::Failure,
                now,
            );
        }

        assert_eq!(limiter.begin(ip(1), "x", now), Ok(()));
    }

    #[test]
    fn one_ip_guessing_many_accounts_is_locked() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        for n in 0..MAX_IP_FAILURES - 1 {
            attempt(
                &mut limiter,
                ip(1),
                &format!("user{n}"),
                Outcome::Failure,
                now,
            );
        }
        let lockouts = attempt(&mut limiter, ip(1), "last", Outcome::Failure, now);

        assert_eq!(
            lockouts,
            NewLockouts {
                ip: true,
                user: false
            }
        );
        assert_eq!(limiter.begin(ip(1), "someone-else", now), Err(LOCKOUT));
        // 其他 IP 不受影響
        assert_eq!(limiter.begin(ip(2), "someone-else", now), Ok(()));
    }

    // ==================== 帳號的計數 ====================

    #[test]
    fn many_ips_guessing_one_account_is_locked_at_a_higher_limit() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        // 每個 IP 只猜一次，不會觸發 IP 的鎖定
        for n in 0..MAX_ACCOUNT_FAILURES - 1 {
            attempt(&mut limiter, ip(n), "alston", Outcome::Failure, now);
        }
        assert_eq!(limiter.begin(ip(1000), "alston", now), Ok(()));
        limiter.end(ip(1000), "alston", Outcome::Neither, now);

        let lockouts = attempt(&mut limiter, ip(1001), "alston", Outcome::Failure, now);

        assert!(lockouts.user);
        assert_eq!(limiter.begin(ip(2000), "alston", now), Err(LOCKOUT));
        // 其他帳號不受影響
        assert_eq!(limiter.begin(ip(2000), "mom", now), Ok(()));
    }

    #[test]
    fn a_single_device_hits_the_ip_limit_long_before_the_account_limit() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        // 一台裝置猜 admin 的密碼：5 次之後這台裝置被擋下，admin 帳號本身沒有被鎖
        for _ in 0..MAX_IP_FAILURES {
            attempt(&mut limiter, ip(1), "admin", Outcome::Failure, now);
        }

        assert!(limiter.begin(ip(1), "admin", now).is_err());
        assert_eq!(limiter.begin(ip(2), "admin", now), Ok(()));
    }

    // ==================== 進行中的嘗試 ====================

    #[test]
    fn concurrent_attempts_count_against_the_limit() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        // 同時開始 5 個嘗試，一個都還沒結束
        for n in 0..MAX_IP_FAILURES {
            assert_eq!(limiter.begin(ip(1), &format!("user{n}"), now), Ok(()));
        }

        // 第 6 個不能開始：前面 5 個若都失敗，就已經達到上限
        assert_eq!(limiter.begin(ip(1), "user9", now), Err(BUSY_RETRY));
    }

    #[test]
    fn finished_attempts_free_their_slots() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        for n in 0..MAX_IP_FAILURES {
            limiter.begin(ip(1), &format!("user{n}"), now).unwrap();
        }
        // 都以「不算失敗」結束：名額歸還，計數不增加
        for n in 0..MAX_IP_FAILURES {
            limiter.end(ip(1), &format!("user{n}"), Outcome::Neither, now);
        }

        assert_eq!(limiter.begin(ip(1), "user9", now), Ok(()));
    }

    #[test]
    fn a_rejected_begin_takes_no_slot() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();
        for n in 0..MAX_IP_FAILURES {
            limiter.begin(ip(1), &format!("user{n}"), now).unwrap();
        }

        // 被拒絕的嘗試沒有佔用帳號的名額：同一個帳號從別的 IP 仍然可以嘗試
        assert!(limiter.begin(ip(1), "alston", now).is_err());
        for n in 0..MAX_ACCOUNT_FAILURES - 1 {
            limiter.begin(ip(100 + n), "alston", now).unwrap();
        }
    }

    // ==================== 時間 ====================

    #[test]
    fn lock_expires_after_lockout() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..MAX_IP_FAILURES {
            attempt(&mut limiter, ip(1), "alston", Outcome::Failure, now);
        }

        let later = now + LOCKOUT;

        assert_eq!(limiter.begin(ip(1), "alston", later), Ok(()));
        // 過期後重新開始計數：再失敗一次不會立刻又被鎖
        limiter.end(ip(1), "alston", Outcome::Failure, later);
        assert_eq!(limiter.begin(ip(1), "alston", later), Ok(()));
    }

    #[test]
    fn remaining_time_counts_down() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();
        for _ in 0..MAX_IP_FAILURES {
            attempt(&mut limiter, ip(1), "alston", Outcome::Failure, now);
        }

        let later = now + Duration::from_secs(60);

        assert_eq!(
            limiter.begin(ip(1), "alston", later),
            Err(LOCKOUT - Duration::from_secs(60))
        );
    }

    #[test]
    fn failures_outside_the_window_do_not_add_up() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        for _ in 0..MAX_IP_FAILURES - 1 {
            attempt(&mut limiter, ip(1), "alston", Outcome::Failure, now);
        }
        for _ in 0..MAX_IP_FAILURES - 1 {
            attempt(
                &mut limiter,
                ip(1),
                "alston",
                Outcome::Failure,
                now + WINDOW,
            );
        }

        assert_eq!(limiter.begin(ip(1), "alston", now + WINDOW), Ok(()));
    }

    // ==================== 登入成功 ====================

    #[test]
    fn success_clears_the_account_but_not_the_ip() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        // 攻擊者在同一個 IP 上猜別人的帳號 4 次
        for n in 0..MAX_IP_FAILURES - 1 {
            attempt(
                &mut limiter,
                ip(1),
                &format!("victim{n}"),
                Outcome::Failure,
                now,
            );
        }
        // 再用自己的帳號登入成功，想把計數歸零
        attempt(&mut limiter, ip(1), "attacker", Outcome::Success, now);
        // 下一次猜錯就達到上限
        attempt(&mut limiter, ip(1), "victim", Outcome::Failure, now);

        assert!(limiter.begin(ip(1), "victim", now).is_err());
    }

    #[test]
    fn success_resets_the_account_count() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        // 從不同裝置打錯很多次之後登入成功：之後重新從零算起
        for n in 0..MAX_ACCOUNT_FAILURES - 1 {
            attempt(&mut limiter, ip(n), "alston", Outcome::Failure, now);
        }
        attempt(&mut limiter, ip(500), "alston", Outcome::Success, now);
        attempt(&mut limiter, ip(501), "alston", Outcome::Failure, now);

        assert_eq!(limiter.begin(ip(502), "alston", now), Ok(()));
    }

    #[test]
    fn neither_does_not_count_or_clear() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        for n in 0..MAX_ACCOUNT_FAILURES - 1 {
            attempt(&mut limiter, ip(n), "alston", Outcome::Failure, now);
        }
        // 密碼正確但帳號停用：不算失敗，也不清除計數
        attempt(&mut limiter, ip(500), "alston", Outcome::Neither, now);
        attempt(&mut limiter, ip(501), "alston", Outcome::Failure, now);

        assert!(limiter.begin(ip(502), "alston", now).is_err());
    }

    // ==================== 計數鍵的正規化 ====================

    #[test]
    fn usernames_are_counted_case_insensitively() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        for n in 0..MAX_ACCOUNT_FAILURES {
            let name = if n % 2 == 0 { "Admin" } else { "admin" };
            attempt(&mut limiter, ip(n), name, Outcome::Failure, now);
        }

        assert!(limiter.begin(ip(500), "ADMIN", now).is_err());
    }

    #[test]
    fn ipv6_addresses_in_the_same_64_share_a_count() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        for n in 0..MAX_IP_FAILURES {
            let addr: Ipv6Addr = format!("2001:db8:1:2::{:x}", n + 1).parse().unwrap();
            attempt(
                &mut limiter,
                IpAddr::V6(addr),
                &format!("user{n}"),
                Outcome::Failure,
                now,
            );
        }

        let same_64: IpAddr = "2001:db8:1:2:ffff::1".parse().unwrap();
        let other_64: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(limiter.begin(same_64, "x", now).is_err());
        assert_eq!(limiter.begin(other_64, "x", now), Ok(()));
    }

    #[test]
    fn ipv4_mapped_ipv6_is_the_same_as_ipv4() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();

        for n in 0..MAX_IP_FAILURES {
            attempt(
                &mut limiter,
                mapped,
                &format!("user{n}"),
                Outcome::Failure,
                now,
            );
        }

        assert!(limiter.begin(ip(1), "x", now).is_err());
    }

    // ==================== 記憶體 ====================

    #[test]
    fn prune_removes_only_entries_without_effect() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();
        attempt(&mut limiter, ip(1), "old", Outcome::Failure, now);
        limiter.begin(ip(2), "in-flight", now).unwrap();

        limiter.prune(now + WINDOW);

        // 視窗已過的失敗被清除；進行中的嘗試保留
        assert!(!limiter.entries.contains_key(&user_key("old")));
        assert!(limiter.entries.contains_key(&user_key("in-flight")));
    }

    #[test]
    fn memory_is_bounded_and_locks_cannot_be_evicted() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();

        // 攻擊者先被鎖住
        for n in 0..MAX_IP_FAILURES {
            attempt(
                &mut limiter,
                ip(1),
                &format!("guess{n}"),
                Outcome::Failure,
                now,
            );
        }
        assert!(limiter.begin(ip(1), "x", now).is_err());

        // 再從大量其他位址送出大量假帳號名稱，想把自己的鎖定擠掉
        for n in 0..(MAX_TRACKED as u32) + 100 {
            let _ = limiter.begin(ip(10 + n), &format!("fake{n}"), now);
            limiter.end(ip(10 + n), &format!("fake{n}"), Outcome::Neither, now);
        }

        assert!(limiter.entries.len() <= MAX_TRACKED);
        // 鎖定仍然有效
        assert!(limiter.begin(ip(1), "x", now).is_err());
    }
}
