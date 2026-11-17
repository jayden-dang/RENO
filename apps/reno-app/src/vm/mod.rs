//! View-models: plain functions from the `reno_client::State` mirror to what a screen shows.
//!
//! None of this needs a window, a clock (callers pass `now_ms`) or a runtime, so every state a
//! screen can be in (loading, empty, offline, failed, live) is a unit test here, and the screens
//! only draw what these return.

pub(crate) mod executors;
pub(crate) mod history;
pub(crate) mod project;
pub(crate) mod projects;
pub(crate) mod routing;
pub(crate) mod rules;
pub(crate) mod runs;
pub(crate) mod settings;
pub(crate) mod triggers;

use reno_client::{ConnectionStatus, FeedState, State};

use crate::store;

/// The strip a screen shows while the daemon is not connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Banner {
    /// What happened.
    pub(crate) reason: String,
    /// Whether and when the app tries again.
    pub(crate) retry: Option<String>,
    /// Whether a connection attempt is running right now.
    pub(crate) connecting: bool,
}

/// The banner for the current connection, `None` while connected.
pub(crate) fn banner(state: &State) -> Option<Banner> {
    match state.connection() {
        ConnectionStatus::Connected => None,
        ConnectionStatus::Connecting if state.offline().is_none() => Some(Banner {
            reason: "Connecting to the daemon…".to_owned(),
            retry: None,
            connecting: true,
        }),
        status => Some(Banner {
            reason: state
                .offline()
                .map_or_else(|| "Not connected.".to_owned(), store::offline_reason),
            retry: store::retry_line(state),
            connecting: status == ConnectionStatus::Connecting,
        }),
    }
}

/// Whether the daemon is known to be out of reach: disconnected, or retrying after a failure.
/// The first connection attempt is not this; a screen is still loading then.
pub(crate) fn is_offline(state: &State) -> bool {
    match state.connection() {
        ConnectionStatus::Connected => false,
        ConnectionStatus::Connecting => state.offline().is_some(),
        ConnectionStatus::Disconnected => true,
    }
}

/// What a list shows, once its feed and rows are known. Empty, loading and failed are different
/// things, and a screen says which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Section<T> {
    /// Nothing has arrived yet.
    Loading,
    /// The daemon is out of reach, so what is in the list, if anything, is unknown.
    NotConnected,
    /// The daemon does not offer this.
    Unavailable,
    /// The last request failed.
    Failed(String),
    /// It arrived, and there is nothing in it.
    Empty,
    /// Rows to show.
    Rows(Vec<T>),
}

impl<T> Section<T> {
    pub(crate) fn of(feed: FeedState, rows: Vec<T>) -> Self {
        if !rows.is_empty() {
            return Self::Rows(rows);
        }
        match feed {
            FeedState::Loading => Self::Loading,
            FeedState::Live => Self::Empty,
            FeedState::Unavailable => Self::Unavailable,
            FeedState::Failed(reason) => Self::Failed(reason),
        }
    }

    /// [`Section::of`] for a screen that knows the connection: with no rows to show and the
    /// daemon out of reach, the list is unknown rather than empty, loading or failed.
    pub(crate) fn of_state(state: &State, feed: FeedState, rows: Vec<T>) -> Self {
        if rows.is_empty() && is_offline(state) {
            return Self::NotConnected;
        }
        Self::of(feed, rows)
    }

    pub(crate) fn rows(&self) -> &[T] {
        match self {
            Self::Rows(rows) => rows,
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use reno_client::{Offline, OfflineKind, Update};

    use super::*;

    #[test]
    fn the_banner_follows_the_connection() {
        let mut state = State::new();
        let b = banner(&state).unwrap();
        assert!(b.connecting && b.retry.is_none());
        assert_eq!(b.reason, "Connecting to the daemon…");

        state.apply(Update::Offline(Offline {
            kind: OfflineKind::NotRunning,
            detail: "x".into(),
            attempt: 3,
        }));
        let b = banner(&state).unwrap();
        assert!(!b.connecting);
        assert_eq!(b.reason, "Daemon not running — run `reno daemon start`");
        assert!(b.retry.unwrap().starts_with("Tried 3 times"));

        state.apply(Update::Connection(ConnectionStatus::Connecting));
        let b = banner(&state).unwrap();
        assert!(b.connecting, "a retry in progress");
        assert_eq!(b.retry.as_deref(), Some("Reconnecting…"));

        state.apply(Update::Connection(ConnectionStatus::Connected));
        assert!(banner(&state).is_none());
    }

    #[test]
    fn a_section_says_why_it_is_empty() {
        assert_eq!(
            Section::<u8>::of(FeedState::Loading, vec![]),
            Section::Loading
        );
        assert_eq!(Section::<u8>::of(FeedState::Live, vec![]), Section::Empty);
        assert_eq!(
            Section::<u8>::of(FeedState::Unavailable, vec![]),
            Section::Unavailable
        );
        assert_eq!(
            Section::<u8>::of(FeedState::Failed("x".into()), vec![]),
            Section::Failed("x".into())
        );
        // Rows win over any feed state: stale rows beat a blank screen.
        assert_eq!(
            Section::of(FeedState::Failed("x".into()), vec![1]),
            Section::Rows(vec![1])
        );
    }
}
