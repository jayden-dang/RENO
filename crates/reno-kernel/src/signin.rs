//! Signing in an added agent, started by a person (`executor.signin.start`).
//!
//! # Why a bounded subscription, not a job
//!
//! A job (`executor.job.*`) is an install or update: steps that end in a version, a log tail kept
//! for a later look, one at a time per executor, published as part of every executor snapshot.
//! A sign-in is a conversation with the person at one device: it hands them a page to open and a
//! code to type, waits for them, and means nothing once they have gone. The events are small,
//! ordered and for one client, so they are a subscription of their own (ARCH-11: a bounded
//! queue, a task the kernel owns). The client that started it is the one that opens the page, and
//! dropping the subscription ends the sign-in and the agent with it, so nothing keeps asking a
//! person who left. What a sign-in leaves behind is the executor's health, which any client reads
//! from the executors watch.
//!
//! # What the daemon does and does not do
//!
//! It runs the agent's own sign-in through the executor port and relays what the agent shows.
//! It never opens a page or a terminal: the address and the command go to the client. It writes
//! no credential and no configuration of the agent (it only reads what the agent says). The
//! sign-in ends with a full probe, which a person asked for, so the health turns signed in.

use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::HarnessError;
use reno_harness::exec::{Executor, ProbeDepth, SignInEvents};
use reno_proto::{ExecutorId, ExecutorSource, SignedIn, SigninEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{KernelError, Result};
use crate::health::Probing;
use crate::maintain::SignInClaim;
use crate::shared::Shared;

/// Events waiting for the client. Output lines are dropped, never queued without bound, when
/// the client does not keep up; the page, the terminal command and the end are not.
const QUEUE: usize = 256;

/// Ends the sign-in when its stream is dropped (the client left or cancelled).
struct EndOnDrop(CancellationToken);

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl crate::Kernel {
    /// `executor.signin.start`: starts signing in the added agent `id` with `method` (an id from
    /// its `auth_methods`; the agent's own first one when absent) and returns what the client is
    /// to show and do, ending with [`SigninEvent::Done`]. Refused for an executor Reno drives
    /// itself, while an install, update or removal of it is running, and while a sign-in of it
    /// already is.
    pub fn start_signin(
        &self,
        id: &ExecutorId,
        method: Option<String>,
    ) -> Result<BoxStream<'static, SigninEvent>> {
        let shared = self.shared();
        let executor = shared
            .executors
            .get(id)
            .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))?;
        if executor.source() != ExecutorSource::Registry {
            return Err(KernelError::SignInRefused {
                executor: id.clone(),
                why: "Reno drives it itself; it signs in with its own command".into(),
            });
        }
        let claim = shared.claim_signin(id)?;
        let (tx, rx) = mpsc::channel(QUEUE);
        let guard = EndOnDrop(claim.cancel.clone());
        shared.spawn(run(
            Arc::clone(shared),
            id.clone(),
            executor,
            method,
            claim,
            tx,
        ));
        Ok(
            futures::stream::unfold((rx, guard), |(mut rx, guard)| async move {
                let event = rx.recv().await?;
                Some((event, (rx, guard)))
            })
            .boxed(),
        )
    }

    /// `executor.signin.cancel`: tells the running sign-in of `id` to stop; its stream then ends
    /// with a [`SigninEvent::Done`] that says it was cancelled. `false` when none is running.
    pub fn cancel_signin(&self, id: &ExecutorId) -> bool {
        self.shared().cancel_signin(id)
    }
}

async fn run(
    shared: Arc<Shared>,
    id: ExecutorId,
    executor: Arc<dyn Executor>,
    method: Option<String>,
    claim: SignInClaim,
    tx: mpsc::Sender<SigninEvent>,
) {
    let sink: SignInEvents = {
        let tx = tx.clone();
        Arc::new(move |event| match event {
            // A line is not worth waiting for; everything else the client must get.
            SigninEvent::Output { .. } => {
                let _ = tx.try_send(event);
            }
            other => {
                if tx.try_send(other).is_err() {
                    tracing::debug!("a sign-in event was dropped: the client is not keeping up");
                }
            }
        })
    };
    tracing::info!(executor = %id, "sign-in started by a person");
    let outcome = tokio::select! {
        biased;
        () = claim.cancel.cancelled() => Err("the sign-in was cancelled".to_owned()),
        outcome = executor.sign_in(method, sink) => outcome.map_err(words),
    };
    let done = match outcome {
        Ok(()) => finish(&shared, &id).await,
        Err(why) => SigninEvent::Done {
            signed_in: false,
            message: Some(why),
        },
    };
    tracing::info!(executor = %id, ?done, "sign-in ended");
    // The claim is held to here: nothing installs, updates or removes the agent mid-probe.
    let _ = tx.send(done).await;
    drop(claim);
}

/// The full probe a sign-in ends with, and what it found.
async fn finish(shared: &Shared, id: &ExecutorId) -> SigninEvent {
    if let Err(error) = shared
        .probe_executor(id, Probing::Fresh, ProbeDepth::Full)
        .await
    {
        tracing::warn!(executor = %id, %error, "the check after a sign-in failed");
    }
    match shared.any_health(id).map(|h| h.signed_in) {
        Some(SignedIn::Yes { .. }) => SigninEvent::Done {
            signed_in: true,
            message: None,
        },
        Some(SignedIn::No { hint }) => SigninEvent::Done {
            signed_in: false,
            message: Some(format!(
                "the sign-in finished, but the agent still says it is signed out: {hint}"
            )),
        },
        _ => SigninEvent::Done {
            signed_in: false,
            message: Some(
                "the sign-in finished, but the check afterwards could not tell whether the agent \
                 is signed in; check it again"
                    .to_owned(),
            ),
        },
    }
}

/// An executor's failure as the person reads it, without the layer that raised it.
fn words(error: HarnessError) -> String {
    match error {
        HarnessError::Protocol(why) | HarnessError::NotSignedIn(why) => why,
        other => other.to_string(),
    }
}
