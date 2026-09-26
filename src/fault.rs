//! Deterministic fault points for crash-path tests. Outside `cfg(test)` every
//! point is a no-op the optimiser removes, so production behaviour carries no
//! injection seam.

use std::io;

#[cfg(test)]
pub use armed::{Fault, arm, arm_nth, disarm, unfired};

/// Fires `point` if a test armed it.
#[cfg(test)]
pub(crate) fn hit(point: &'static str) -> io::Result<()> {
    match armed::take(point) {
        None => Ok(()),
        Some(Fault::Fail) => Err(io::Error::other(format!("injected fault at {point}"))),
        Some(Fault::Crash) => panic!("injected crash at {point}"),
    }
}

#[cfg(not(test))]
#[inline(always)]
pub(crate) fn hit(_point: &'static str) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod armed {
    use std::cell::RefCell;

    /// What an armed point does when execution reaches it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Fault {
        /// The point returns an I/O error, as a real failing syscall would.
        Fail,
        /// The point panics: the op stops mid-flight with no cleanup, which
        /// is what a killed process leaves on disk.
        Crash,
    }

    /// An armed point: how many hits to let pass first, and what to do.
    type Armed = (&'static str, usize, Fault);

    thread_local! {
        static ARMED: RefCell<Vec<Armed>> = const { RefCell::new(Vec::new()) };
    }

    /// Arms `point` to fire once, on its first hit.
    pub fn arm(point: &'static str, fault: Fault) {
        arm_nth(point, 1, fault);
    }

    /// Arms `point` to fire once, on its `nth` hit (1-based). Faults are
    /// per thread, so parallel tests never see each other's.
    pub fn arm_nth(point: &'static str, nth: usize, fault: Fault) {
        ARMED.with(|a| a.borrow_mut().push((point, nth.saturating_sub(1), fault)));
    }

    /// Disarms every point on this thread.
    pub fn disarm() {
        ARMED.with(|a| a.borrow_mut().clear());
    }

    /// The points armed on this thread that have not fired: a test whose
    /// fault never fired proved nothing.
    pub fn unfired() -> Vec<&'static str> {
        ARMED.with(|a| a.borrow().iter().map(|(p, _, _)| *p).collect())
    }

    pub(super) fn take(point: &str) -> Option<Fault> {
        ARMED.with(|a| {
            let mut a = a.borrow_mut();
            let pos = a.iter().position(|(p, _, _)| *p == point)?;
            if a[pos].1 > 0 {
                a[pos].1 -= 1;
                return None;
            }
            Some(a.remove(pos).2)
        })
    }
}
