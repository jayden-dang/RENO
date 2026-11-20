//! The fake daemon's side of the mocked message-queue contract (see [`crate::queue`]). The busy
//! demo run `plan1` starts with two messages waiting. Time is whatever
//! [`FakeDaemon::advance_queue`] is given: each time a turn "ends" (every 30 s of it) the first
//! message of a run's queue is delivered into the transcript. `send_now` delivers at once.

use std::collections::HashMap;
use std::sync::Mutex;

use reno_proto::RunId;

use super::FakeDaemon;
use crate::queue::{MessageQueue, QueueError, QueuedMessage};

/// A turn ends this often.
pub(super) const TURN_MS: u64 = 30_000;

#[derive(Debug, Default)]
struct Queue {
    messages: Vec<QueuedMessage>,
    since_turn_ms: u64,
}

#[derive(Debug, Default)]
struct Inner {
    runs: HashMap<RunId, Queue>,
    next_id: u64,
}

/// The queues by run.
#[derive(Debug, Default)]
pub(super) struct Sim(Mutex<Inner>);

impl Inner {
    fn mint(&mut self, text: &str) -> QueuedMessage {
        self.next_id += 1;
        QueuedMessage {
            id: format!("q{}", self.next_id),
            text: text.to_owned(),
        }
    }
}

pub(super) fn seed(on: bool) -> Sim {
    let sim = Sim::default();
    if on && let Ok(run) = RunId::from_suffix(super::activity::BUSY_RUN) {
        let mut inner = sim
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let first = inner.mint("Fix the flaky handshake test before you open the PR");
        let second = inner.mint("Then update the conformance fixtures and open the pull request");
        inner.runs.entry(run).or_default().messages = vec![first, second];
    }
    sim
}

impl FakeDaemon {
    fn run_state(&self, run: &RunId) -> Result<(), QueueError> {
        match self
            .shared
            .read(|w| w.run(run).map(super::world::RunRecord::finished))
        {
            None => Err(QueueError::UnknownRun),
            Some(true) => Err(QueueError::RunEnded),
            Some(false) => Ok(()),
        }
    }

    /// Journals `text` as the owner's message to the live run.
    fn deliver(&self, run: &RunId, text: &str) {
        self.shared.with(|w| w.steer(run, text));
    }

    fn queues(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.queue
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Moves the queues' clock on by `elapsed_ms`. Every [`TURN_MS`] a live run's turn ends and
    /// its first waiting message is delivered. Returns what was delivered, for tests.
    pub fn advance_queue(&self, elapsed_ms: u64) -> Vec<(RunId, QueuedMessage)> {
        let due: Vec<(RunId, QueuedMessage)> = {
            let mut inner = self.queues();
            let mut due = Vec::new();
            for (run, queue) in &mut inner.runs {
                if queue.messages.is_empty() {
                    queue.since_turn_ms = 0;
                    continue;
                }
                queue.since_turn_ms += elapsed_ms;
                if queue.since_turn_ms >= TURN_MS {
                    queue.since_turn_ms -= TURN_MS;
                    due.push((run.clone(), queue.messages.remove(0)));
                }
            }
            due
        };
        for (run, message) in &due {
            self.deliver(run, &message.text);
        }
        due
    }
}

impl MessageQueue for FakeDaemon {
    fn queue(&self, run: &RunId) -> Vec<QueuedMessage> {
        self.queues()
            .runs
            .get(run)
            .map(|q| q.messages.clone())
            .unwrap_or_default()
    }

    fn queue_add(&self, run: &RunId, text: &str) -> Result<QueuedMessage, QueueError> {
        self.run_state(run)?;
        let text = text.trim();
        if text.is_empty() {
            return Err(QueueError::Empty);
        }
        let mut inner = self.queues();
        let message = inner.mint(text);
        inner
            .runs
            .entry(run.clone())
            .or_default()
            .messages
            .push(message.clone());
        Ok(message)
    }

    fn queue_edit(&self, run: &RunId, id: &str, text: &str) -> Result<(), QueueError> {
        self.run_state(run)?;
        let text = text.trim();
        if text.is_empty() {
            return Err(QueueError::Empty);
        }
        let mut inner = self.queues();
        let message = inner
            .runs
            .get_mut(run)
            .and_then(|q| q.messages.iter_mut().find(|m| m.id == id))
            .ok_or(QueueError::UnknownMessage)?;
        text.clone_into(&mut message.text);
        Ok(())
    }

    fn queue_move(&self, run: &RunId, id: &str, to: usize) -> Result<(), QueueError> {
        self.run_state(run)?;
        let mut inner = self.queues();
        let messages = &mut inner
            .runs
            .get_mut(run)
            .ok_or(QueueError::UnknownMessage)?
            .messages;
        let from = messages
            .iter()
            .position(|m| m.id == id)
            .ok_or(QueueError::UnknownMessage)?;
        let message = messages.remove(from);
        messages.insert(to.min(messages.len()), message);
        Ok(())
    }

    fn queue_remove(&self, run: &RunId, id: &str) -> Result<(), QueueError> {
        self.run_state(run)?;
        let mut inner = self.queues();
        let messages = &mut inner
            .runs
            .get_mut(run)
            .ok_or(QueueError::UnknownMessage)?
            .messages;
        let at = messages
            .iter()
            .position(|m| m.id == id)
            .ok_or(QueueError::UnknownMessage)?;
        messages.remove(at);
        Ok(())
    }

    fn queue_send_now(&self, run: &RunId, id: &str) -> Result<QueuedMessage, QueueError> {
        self.run_state(run)?;
        let message = {
            let mut inner = self.queues();
            let messages = &mut inner
                .runs
                .get_mut(run)
                .ok_or(QueueError::UnknownMessage)?
                .messages;
            let at = messages
                .iter()
                .position(|m| m.id == id)
                .ok_or(QueueError::UnknownMessage)?;
            messages.remove(at)
        };
        self.deliver(run, &message.text);
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> (FakeDaemon, RunId) {
        let daemon = FakeDaemon::start(super::super::FakeConfig {
            activity_runs: true,
            ..super::super::FakeConfig::default()
        })
        .unwrap();
        (
            daemon,
            RunId::from_suffix(super::super::activity::BUSY_RUN).unwrap(),
        )
    }

    fn texts(daemon: &FakeDaemon, run: &RunId) -> Vec<String> {
        daemon.queue(run).into_iter().map(|m| m.text).collect()
    }

    #[test]
    fn the_busy_run_starts_with_two_waiting_and_other_runs_with_none() {
        let (daemon, run) = busy();
        assert_eq!(daemon.queue(&run).len(), 2);
        assert!(
            daemon
                .queue(&RunId::from_suffix("plan0").unwrap())
                .is_empty()
        );
    }

    #[test]
    fn add_appends_edit_keeps_the_place_and_move_reorders_by_id() {
        let (daemon, run) = busy();
        let third = daemon.queue_add(&run, "  Add a changelog line  ").unwrap();
        assert_eq!(third.text, "Add a changelog line");
        let ids: Vec<String> = daemon.queue(&run).into_iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), 3);
        daemon.queue_edit(&run, &ids[0], "Fix the test").unwrap();
        daemon.queue_move(&run, &third.id, 0).unwrap();
        assert_eq!(
            daemon
                .queue(&run)
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>(),
            [ids[2].clone(), ids[0].clone(), ids[1].clone()]
        );
        assert_eq!(texts(&daemon, &run)[1], "Fix the test");
        // Past the end is last; an unknown id is refused; blank text is refused.
        daemon.queue_move(&run, &ids[2], 99).unwrap();
        assert_eq!(daemon.queue(&run)[2].id, ids[2]);
        assert_eq!(
            daemon.queue_move(&run, "nope", 0),
            Err(QueueError::UnknownMessage)
        );
        assert_eq!(daemon.queue_add(&run, "   "), Err(QueueError::Empty));
        assert_eq!(daemon.queue_edit(&run, &ids[0], ""), Err(QueueError::Empty));
    }

    #[test]
    fn one_message_is_delivered_per_turn_end_in_order() {
        let (daemon, run) = busy();
        let before = texts(&daemon, &run);
        assert!(daemon.advance_queue(29_000).is_empty());
        let first = daemon.advance_queue(1_000);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1.text, before[0]);
        assert_eq!(texts(&daemon, &run), [before[1].clone()]);
        // The next turn end delivers the next; an empty queue delivers nothing.
        assert_eq!(daemon.advance_queue(30_000)[0].1.text, before[1]);
        assert!(daemon.queue(&run).is_empty());
        assert!(daemon.advance_queue(90_000).is_empty());
    }

    #[test]
    fn a_delivered_message_reaches_the_transcript() {
        let (daemon, run) = busy();
        let first = daemon.queue(&run)[0].clone();
        daemon.advance_queue(30_000);
        let journalled = daemon.shared.read(|w| {
            w.run(&run).map(|r| {
                r.journal.iter().any(|e| {
                    matches!(e, reno_proto::ExecEvent::UserMessage { text } if *text == first.text)
                })
            })
        });
        assert_eq!(journalled, Some(true));
    }

    #[test]
    fn send_now_removes_the_message_and_delivers_it_at_once() {
        let (daemon, run) = busy();
        let second = daemon.queue(&run)[1].clone();
        let sent = daemon.queue_send_now(&run, &second.id).unwrap();
        assert_eq!(sent, second);
        assert_eq!(daemon.queue(&run).len(), 1);
        assert_eq!(
            daemon.queue_send_now(&run, &second.id),
            Err(QueueError::UnknownMessage)
        );
    }

    #[test]
    fn remove_takes_a_message_back_without_sending_it() {
        let (daemon, run) = busy();
        let id = daemon.queue(&run)[0].id.clone();
        daemon.queue_remove(&run, &id).unwrap();
        assert_eq!(daemon.queue(&run).len(), 1);
        assert_eq!(
            daemon.queue_remove(&run, &id),
            Err(QueueError::UnknownMessage)
        );
    }

    #[test]
    fn an_ended_run_refuses_every_change_and_says_so() {
        let (daemon, _) = busy();
        // A seeded run that ended: the fake's demo data has finished runs.
        let ended = daemon
            .shared
            .read(|w| w.summaries())
            .into_iter()
            .find(|s| s.status.is_terminal())
            .map(|s| s.run_id)
            .unwrap();
        assert_eq!(daemon.queue_add(&ended, "hello"), Err(QueueError::RunEnded));
        assert_eq!(
            QueueError::RunEnded.to_string(),
            "The run ended; send it as a new message instead"
        );
        assert_eq!(
            daemon.queue_send_now(&ended, "q1"),
            Err(QueueError::RunEnded)
        );
        let unknown = RunId::from_suffix("nothere").unwrap();
        assert_eq!(daemon.queue_add(&unknown, "x"), Err(QueueError::UnknownRun));
    }
}
