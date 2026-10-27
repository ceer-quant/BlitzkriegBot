#![cfg(test)]

use super::*;

/// P0 #670: accept Err 臂的限速/退避纯逻辑钉。用合成时钟（Instant 加法）
/// 驱动 `step`，绝不 sleep。
mod accept_err_state {
    use super::*;

    fn at(ms: u64) -> std::time::Instant {
        // 一个公共起点 + 偏移；Instant::checked_add 对任意进程内时间轴成立。
        std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(ms))
            .expect("synthetic clock offset in range")
    }

    #[test]
    fn same_kind_within_5s_window_is_silent_and_counted() {
        let mut s = AcceptErrState::default();
        let kind = std::io::ErrorKind::Other;
        assert!(s.step(kind, at(0)).0, "first hit always logs");
        // 5s 窗内的每一次都静默，抑制计数递增。
        for i in 1..=100u64 {
            let (should_log, suppressed) = s.step(kind, at(i * 40));
            assert!(!should_log, "hit {i} within the window must be silenced");
            assert_eq!(suppressed, i, "suppression count tracks each silenced hit");
        }
    }

    #[test]
    fn crossing_the_5s_window_logs_again_and_flushes_the_count() {
        let mut s = AcceptErrState::default();
        let kind = std::io::ErrorKind::Other;
        assert!(s.step(kind, at(0)).0);
        // 窗内静默一条（t=100 < 5000），随后跨窗放行时随行补记。
        assert!(!s.step(kind, at(100)).0);
        let (should_log, suppressed) = s.step(kind, at(5_100));
        assert!(should_log, "past the window the same kind logs again");
        assert_eq!(
            suppressed, 1,
            "the one silenced hit is reported with the next log"
        );
        // 刚放行后又立刻来一条：新窗口重新静默。
        let (should_log, suppressed) = s.step(kind, at(5_200));
        assert!(!should_log);
        assert_eq!(suppressed, 1);
    }

    #[test]
    fn different_kind_logs_immediately() {
        let mut s = AcceptErrState::default();
        let a = std::io::ErrorKind::Other;
        let b = std::io::ErrorKind::ConnectionAborted;
        assert!(s.step(a, at(0)).0);
        let (should_log, suppressed) = s.step(b, at(10));
        assert!(should_log, "a different ErrorKind bypasses the window");
        assert_eq!(
            suppressed, 0,
            "the other kind's window owns the suppressed counter at switch time"
        );
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let mut s = AcceptErrState::default();
        let kind = std::io::ErrorKind::Other;
        // 首次放行 → 50；其后每次放行（跨 5s 窗）倍增到 2000 封顶。
        let mut now = 0u64;
        let expected = [50u64, 100, 200, 400, 800, 1600, 2000, 2000];
        for want in expected {
            let (should_log, _) = s.step(kind, at(now));
            assert!(should_log, "window crossings log (and step the backoff)");
            assert_eq!(s.backoff_ms, want, "backoff sequence 50→100→…→2000");
            now += 5_100;
        }
    }

    #[test]
    fn ok_resets_everything() {
        let mut s = AcceptErrState::default();
        let kind = std::io::ErrorKind::Other;
        assert!(s.step(kind, at(0)).0);
        assert!(!s.step(kind, at(100)).0);
        s.reset_ok();
        assert_eq!(
            s.backoff_ms, ACCEPT_BACKOFF_START_MS,
            "backoff re-arms at 50"
        );
        assert!(s.last_logged.is_none(), "rate-limit memory cleared");
        // 复位后同种类立刻放行，suppressed 从 0 起。
        let (should_log, suppressed) = s.step(kind, at(200));
        assert!(should_log, "post-reset the same kind logs immediately");
        assert_eq!(suppressed, 0, "post-reset the suppressed counter restarts");
    }

    #[test]
    fn emfile_errno_is_pinned_on_both_platforms() {
        // Linux 与 macOS 的 EMFILE 都是 24——修②b 的 RiskAlert 判据钉死。
        assert_eq!(ACCEPT_EMFILE_ERRNO, libc::EMFILE);
    }
}
