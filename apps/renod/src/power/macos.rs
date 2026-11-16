//! IOKit system-power registration over a private dispatch queue.
#![expect(
    unsafe_code,
    reason = "IOKit and libdispatch are C APIs: a handful of extern calls, wrapped here and nowhere else"
)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use reno_kernel::WakeSignal;

use super::{Message, PowerError, classify};

type MachPort = u32;
type IoObject = u32;
type IoReturn = i32;
type NotifyPort = *mut c_void;
type DispatchQueue = *mut c_void;
type PowerCallback = extern "C" fn(*mut c_void, IoObject, u32, *mut c_void);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut NotifyPort,
        callback: PowerCallback,
        notifier: *mut IoObject,
    ) -> MachPort;
    fn IODeregisterForSystemPower(notifier: *mut IoObject) -> IoReturn;
    fn IOAllowPowerChange(kernel_port: MachPort, notification_id: isize) -> IoReturn;
    fn IOServiceClose(connect: MachPort) -> IoReturn;
    fn IONotificationPortSetDispatchQueue(port: NotifyPort, queue: DispatchQueue);
    fn IONotificationPortDestroy(port: NotifyPort);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_release(object: *mut c_void);
}

/// What the callback needs. Boxed, so its address is stable for the registration's life.
struct Shared {
    root_port: AtomicU32,
    signal: WakeSignal,
}

extern "C" fn on_power(
    refcon: *mut c_void,
    _service: IoObject,
    message: u32,
    argument: *mut c_void,
) {
    // SAFETY: `refcon` is the `Shared` that `Registration` boxed; it outlives the registration
    // because `Drop` waits for the queue to drain before freeing it.
    let shared = unsafe { &*refcon.cast::<Shared>() };
    match classify(message) {
        Message::Acknowledge => {
            // SAFETY: the root port and the notification id come from IOKit for this message.
            unsafe {
                IOAllowPowerChange(shared.root_port.load(Ordering::Acquire), argument as isize);
            }
        }
        Message::Woke => shared.signal.notify(),
        Message::Ignore => {}
    }
}

extern "C" fn barrier(_: *mut c_void) {}

/// A live registration.
pub(super) struct Registration {
    shared: *mut Shared,
    port: NotifyPort,
    notifier: IoObject,
    root_port: MachPort,
    queue: DispatchQueue,
}

// SAFETY: the raw pointers are only used by `new` and `drop`, one thread at a time; the callback
// reaches `Shared` through the queue and `Shared` is `Send + Sync`.
unsafe impl Send for Registration {}

impl Registration {
    pub(super) fn new(signal: WakeSignal) -> Result<Self, PowerError> {
        let shared = Box::into_raw(Box::new(Shared {
            root_port: AtomicU32::new(0),
            signal,
        }));
        let mut port: NotifyPort = std::ptr::null_mut();
        let mut notifier: IoObject = 0;
        // SAFETY: the out-pointers are valid; `shared` stays alive until `drop`.
        let root_port = unsafe {
            IORegisterForSystemPower(shared.cast(), &raw mut port, on_power, &raw mut notifier)
        };
        if root_port == 0 || port.is_null() {
            // SAFETY: nothing else has seen `shared`.
            drop(unsafe { Box::from_raw(shared) });
            return Err(PowerError::Register);
        }
        // SAFETY: `shared` is valid.
        unsafe { (*shared).root_port.store(root_port, Ordering::Release) };
        // SAFETY: a serial queue with a static label.
        let queue =
            unsafe { dispatch_queue_create(c"reno.renod.power".as_ptr(), std::ptr::null()) };
        // SAFETY: `port` and `queue` are valid; callbacks start here.
        unsafe { IONotificationPortSetDispatchQueue(port, queue) };
        Ok(Self {
            shared,
            port,
            notifier,
            root_port,
            queue,
        })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: `new` undone in reverse. Deregistering and destroying the port stop new
        // callbacks; the empty synchronous block waits for one already running; only then is
        // `Shared` freed.
        unsafe {
            IODeregisterForSystemPower(&raw mut self.notifier);
            IOServiceClose(self.root_port);
            IONotificationPortDestroy(self.port);
            dispatch_sync_f(self.queue, std::ptr::null_mut(), barrier);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.shared));
        }
    }
}
