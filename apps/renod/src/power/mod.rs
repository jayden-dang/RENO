//! The system's sleep and wake notifications, as a signal for the scheduler (ARCH-10).
//!
//! The scheduler's timer is monotonic and stops while the Mac sleeps, so a trigger due during
//! the sleep would otherwise fire late. This module listens for the system waking
//! and raises the kernel's [`WakeSignal`]; the scheduler then reads the wall clock and fires
//! whatever came due. Nothing here polls: IOKit calls back on its own queue.
//!
//! On macOS the mechanism is IOKit's `IORegisterForSystemPower` with a dispatch queue (see
//! `macos.rs`; a few dozen lines of `extern "C"`, the module's only `unsafe`). The same callback
//! must acknowledge the system's "can I sleep?" and "will sleep" messages, or the machine's
//! sleep is delayed by 30 seconds, so it does. Other platforms have no source: [`watch`] reports
//! [`PowerError::Unsupported`] and the daemon carries on without one.

use reno_kernel::WakeSignal;

#[cfg(target_os = "macos")]
mod macos;

/// What a power message means to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum Message {
    /// The system asks permission to sleep, or announces it will: it must be answered, or sleep
    /// waits for a timeout.
    Acknowledge,
    /// The system has powered on again.
    Woke,
    /// Anything else.
    Ignore,
}

/// `kIOMessageCanSystemSleep`.
const CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
/// `kIOMessageSystemWillSleep`.
const SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
/// `kIOMessageSystemHasPoweredOn`.
const SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;

/// Classifies an IOKit power message.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn classify(message_type: u32) -> Message {
    match message_type {
        CAN_SYSTEM_SLEEP | SYSTEM_WILL_SLEEP => Message::Acknowledge,
        SYSTEM_HAS_POWERED_ON => Message::Woke,
        _ => Message::Ignore,
    }
}

/// Why wake notifications could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum PowerError {
    /// The system refused the registration.
    #[error("cannot register for system power notifications")]
    Register,
    /// This platform has no wake source.
    #[error("system wake notifications are not available on this platform")]
    Unsupported,
}

/// A running registration. Dropping it unregisters and waits for any callback in flight, so the
/// signal is never raised after the drop returns.
#[must_use = "dropping the watch stops the wake notifications"]
pub struct PowerWatch {
    #[cfg(target_os = "macos")]
    _inner: macos::Registration,
}

impl std::fmt::Debug for PowerWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerWatch").finish_non_exhaustive()
    }
}

/// Starts raising `signal` whenever the system wakes from sleep.
pub fn watch(signal: WakeSignal) -> Result<PowerWatch, PowerError> {
    #[cfg(target_os = "macos")]
    {
        macos::Registration::new(signal).map(|inner| PowerWatch { _inner: inner })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = signal;
        Err(PowerError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_power_on_is_a_wake_and_the_sleep_questions_are_answered() {
        assert_eq!(classify(0xE000_0300), Message::Woke);
        assert_eq!(classify(0xE000_0270), Message::Acknowledge);
        assert_eq!(classify(0xE000_0280), Message::Acknowledge);
        // `WillPowerOn` comes before the hardware is up; `HasPoweredOn` is the one that counts.
        assert_eq!(classify(0xE000_0320), Message::Ignore);
        assert_eq!(classify(0xE000_0290), Message::Ignore);
        assert_eq!(classify(0), Message::Ignore);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn registering_and_dropping_leaves_nothing_behind() {
        let signal = WakeSignal::new();
        for _ in 0..3 {
            let watch = watch(signal.clone()).expect("register");
            drop(watch);
        }
    }
}
