//! Delayed quit with re-check: only after the delay elapses is the window
//! count verified again; a new window in the meantime (generation counter)
//! aborts. Always graceful terminate, never force.

use std::ptr::NonNull;
use std::rc::Weak;

use block2::RcBlock;
use objc2_foundation::NSTimer;

use super::Engine;

pub fn schedule(engine: Weak<Engine>, pid: libc::pid_t, generation: u64, delay_secs: f64) {
    tracing::debug!(pid, delay_secs, "quit scheduled");
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        super::catch_callback_panic("quit timer", || {
            let Some(engine) = engine.upgrade() else {
                return;
            };
            engine.finish_quit(pid, generation);
        });
    });
    let _ =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay_secs, false, &block) };
}

pub fn schedule_pending_expiry(
    engine: Weak<Engine>,
    pid: libc::pid_t,
    generation: u64,
    delay_secs: f64,
) {
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        super::catch_callback_panic("termination expiry timer", || {
            if let Some(engine) = engine.upgrade() {
                engine.expire_pending_termination(pid, generation);
            }
        });
    });
    let _ =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay_secs, false, &block) };
}
