//! The brain of the daemon: dispatcher, leases, router, triggers, flows, boards, policy and
//! evaluation, behind one façade.
//!
//! Owns ARCH-7: the kernel does no I/O of its own. It reaches the world only through `reno-store`
//! and its ports (`Clock`, `Executor`, `Workspace`, `Watch`, `Definitions`, `Secrets`), which are
//! implemented outside it and faked in tests. Owns ARCH-10 and ARCH-11: nothing polls, every
//! queue is bounded and every task is owned.
