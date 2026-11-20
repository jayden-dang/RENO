//! A thread's link, `reno://thread/<thread id>` (ROAD-38): what the row menu's "Copy link" puts
//! on the clipboard and what `RootView::open_link` takes. Registering the URL scheme with the OS
//! needs the app bundle and is not done here; the app only understands the link when it is
//! handed one.

use reno_proto::ThreadId;

const PREFIX: &str = "reno://thread/";

/// The link to `thread`.
pub(crate) fn link(thread: &ThreadId) -> String {
    format!("{PREFIX}{thread}")
}

/// The thread a link names. Strict: the exact scheme and path, one thread id, nothing after it.
pub(crate) fn parse(url: &str) -> Option<ThreadId> {
    url.strip_prefix(PREFIX)?.parse().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_link_names_the_thread_and_reads_back() {
        let thread = ThreadId::from_suffix("plan1").unwrap();
        assert_eq!(link(&thread), "reno://thread/thr_plan1");
        assert_eq!(parse(&link(&thread)), Some(thread));
    }

    #[test]
    fn anything_else_is_not_a_thread_link() {
        for url in [
            "",
            "reno://thread/",
            "reno://thread/plan1",
            "reno://thread/thr_plan1/extra",
            "reno://thread/thr_plan1?x=1",
            "reno://thread/thr_plan 1",
            " reno://thread/thr_plan1",
            "reno://run/run_plan1",
            "https://thread/thr_plan1",
            "RENO://thread/thr_plan1",
            "reno://thread/run_plan1",
        ] {
            assert_eq!(parse(url), None, "{url:?}");
        }
    }
}
