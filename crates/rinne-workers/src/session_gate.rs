//! Limit concurrent visible (PTY) harness sessions (`plan.md` Phase 5).
//!
//! Cap comes from env `RINNE_HARNESS_STAGE_MAX` (set from `[harness_stage].max_sessions`).

use std::sync::atomic::{AtomicUsize, Ordering};

static ACTIVE_VISIBLE: AtomicUsize = AtomicUsize::new(0);

/// RAII guard: holding one slot for a visible harness Stage session.
pub struct VisibleSlot {
    _private: (),
}

impl Drop for VisibleSlot {
    fn drop(&mut self) {
        ACTIVE_VISIBLE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn max_visible() -> usize {
    std::env::var("RINNE_HARNESS_STAGE_MAX")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(3)
}

/// Try to reserve a visible Stage slot. `None` means the cap is full — caller
/// should fall back to headless and narrate.
pub fn try_acquire_visible() -> Option<VisibleSlot> {
    let max = max_visible();
    loop {
        let cur = ACTIVE_VISIBLE.load(Ordering::SeqCst);
        if cur >= max {
            return None;
        }
        if ACTIVE_VISIBLE
            .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(VisibleSlot { _private: () });
        }
    }
}

/// How many visible sessions are currently open (tests / status).
#[cfg(test)]
pub fn active_count() -> usize {
    ACTIVE_VISIBLE.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests below: the gate's counter and `RINNE_HARNESS_STAGE_MAX`
    /// are process-global, so concurrent tests would see each other's slots.
    static GATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn gate_enforces_cap() {
        let _guard = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("RINNE_HARNESS_STAGE_MAX", "2");
        assert_eq!(active_count(), 0, "another test leaked a slot");

        let a = try_acquire_visible();
        let b = try_acquire_visible();
        assert!(a.is_some() && b.is_some(), "both slots must be grantable");
        assert_eq!(active_count(), 2);

        // The cap is the point: the third caller runs headless instead.
        assert!(
            try_acquire_visible().is_none(),
            "a third visible session must be refused at max = 2"
        );

        drop(a);
        assert_eq!(active_count(), 1, "dropping a slot must release it");
        let c = try_acquire_visible();
        assert!(c.is_some(), "a freed slot must be reusable");

        drop(b);
        drop(c);
        assert_eq!(active_count(), 0, "every slot must be returned");
        std::env::remove_var("RINNE_HARNESS_STAGE_MAX");
    }
}
