//! The Terminal tab's methods on [`Client`]: `term.open`, `term.write`, `term.resize`,
//! `term.close`, `term.status`, `term.list` and the `term.attach` subscription.
//!
//! All need feature `terminals`: on a daemon without it they refuse up front, without sending
//! anything. They are person-only (a run's token is refused by the daemon), and the state mirror
//! holds nothing for them: a caller asks when it needs an answer. Terminals die with the daemon,
//! so a reconnect after a restart finds none: `term.list` is empty and an attach is `NotFound`.

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reno_proto::{
    FEATURE_TERMINALS, RunId, TermAttach, TermAttachParams, TermClose, TermEvent, TermId, TermInfo,
    TermList, TermListParams, TermOpen, TermOpenParams, TermOpened, TermRef, TermResize,
    TermResizeParams, TermStatus, TermStatusResult, TermWrite, TermWriteParams, encode_bytes,
};
use reno_rpc::{SubEvent, SubStream};

use super::{Client, MAX_STALLED_GAPS};
use crate::error::ClientError;

impl Client {
    /// `term.open`: a shell in the run's worktree. Needs feature `terminals`.
    pub async fn term_open(&self, params: TermOpenParams) -> Result<TermOpened, ClientError> {
        self.require(FEATURE_TERMINALS)?;
        Ok(self.rpc.call::<TermOpen>(params).await?)
    }

    /// `term.write`: input for the shell, in the order the daemon accepts it. Keep one write in
    /// flight: await it before the next. Needs feature `terminals`.
    pub async fn term_write(&self, id: TermId, bytes: &[u8]) -> Result<(), ClientError> {
        self.require(FEATURE_TERMINALS)?;
        self.rpc
            .call::<TermWrite>(TermWriteParams {
                id,
                data: encode_bytes(bytes),
            })
            .await?;
        Ok(())
    }

    /// `term.resize`. Needs feature `terminals`.
    pub async fn term_resize(&self, id: TermId, cols: u16, rows: u16) -> Result<(), ClientError> {
        self.require(FEATURE_TERMINALS)?;
        self.rpc
            .call::<TermResize>(TermResizeParams { id, cols, rows })
            .await?;
        Ok(())
    }

    /// `term.close`: ends the shell and forgets the terminal. Needs feature `terminals`.
    pub async fn term_close(&self, id: TermId) -> Result<(), ClientError> {
        self.require(FEATURE_TERMINALS)?;
        self.rpc.call::<TermClose>(TermRef { id }).await?;
        Ok(())
    }

    /// `term.status`: whether closing would end a program, whether the shell exited, and whether
    /// it is sandboxed. Needs feature `terminals`.
    pub async fn term_status(&self, id: TermId) -> Result<TermStatusResult, ClientError> {
        self.require(FEATURE_TERMINALS)?;
        Ok(self.rpc.call::<TermStatus>(TermRef { id }).await?)
    }

    /// `term.list`: the terminals in the run's worktree. Needs feature `terminals`.
    pub async fn term_list(&self, run_id: RunId) -> Result<Vec<TermInfo>, ClientError> {
        self.require(FEATURE_TERMINALS)?;
        Ok(self
            .rpc
            .call::<TermList>(TermListParams { run_id })
            .await?
            .terminals)
    }

    /// `term.attach`: the terminal's output after `after_seq` (everything the replay window
    /// holds when `None`), then live, ending after the shell's `Exit`. Dropping the stream
    /// detaches and leaves the shell running.
    ///
    /// When the daemon cuts the subscription off with `sub.gap` (ARCH-12) the stream attaches
    /// again from the last item it delivered, and the daemon's own `Gap` item tells the viewer
    /// if output was lost in between. Gaps in a row with nothing delivered between them end the
    /// stream with [`ClientError::Protocol`]. Needs feature `terminals`.
    pub async fn term_attach(
        &self,
        id: TermId,
        after_seq: Option<u64>,
    ) -> Result<BoxStream<'static, Result<TermEvent, ClientError>>, ClientError> {
        self.require(FEATURE_TERMINALS)?;
        let params = TermAttachParams { id, after_seq };
        let sub = self.rpc.subscribe::<TermAttach>(params.clone()).await?;
        let state = AttachState {
            rpc: self.rpc.clone(),
            params,
            sub: Some(sub),
            stalled: 0,
            failed: false,
        };
        Ok(stream::unfold(state, AttachState::step).boxed())
    }
}

struct AttachState {
    rpc: reno_rpc::Client,
    params: TermAttachParams,
    sub: Option<SubStream<TermEvent>>,
    stalled: u32,
    failed: bool,
}

impl AttachState {
    async fn step(mut self) -> Option<(Result<TermEvent, ClientError>, Self)> {
        if self.failed {
            return None;
        }
        match self.advance().await {
            Ok(Some(item)) => Some((Ok(item), self)),
            Ok(None) => None,
            Err(error) => {
                self.failed = true;
                Some((Err(error), self))
            }
        }
    }

    async fn advance(&mut self) -> Result<Option<TermEvent>, ClientError> {
        loop {
            let sub = match self.sub.as_mut() {
                Some(sub) => sub,
                None => self.sub.insert(
                    self.rpc
                        .subscribe::<TermAttach>(self.params.clone())
                        .await?,
                ),
            };
            match sub.next().await {
                Some(SubEvent::Item(item)) => {
                    self.stalled = 0;
                    // A gap item stands for what came before the item that follows it.
                    self.params.after_seq = Some(match &item {
                        TermEvent::Gap { seq, .. } => seq.saturating_sub(1),
                        other => other.seq(),
                    });
                    return Ok(Some(item));
                }
                Some(SubEvent::Gap { resume_from }) => {
                    self.sub = None;
                    self.params.after_seq = Some(resume_from.0.saturating_sub(1));
                    self.stalled += 1;
                    if self.stalled > MAX_STALLED_GAPS {
                        return Err(ClientError::Protocol(
                            "a terminal keeps gapping without delivering output".to_owned(),
                        ));
                    }
                }
                Some(SubEvent::Done) | None => return Ok(None),
            }
        }
    }
}
