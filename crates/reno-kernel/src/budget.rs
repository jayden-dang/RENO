//! Budgets (T1.7): what the kernel does with a bot's `budget` and with the kill switch.
//!
//! # Where each limit is enforced
//!
//! | Limit | Enforced | A run that hits it |
//! |---|---|---|
//! | `max_concurrent_runs` | in the claim (`reno_store::ClaimFilter`), against the definition version the run is pinned to | stays `queued`; the end of a run of the bot wakes the dispatcher, so it is claimed when a slot frees |
//! | `max_wall_time` | a deadline in the run's supervisor, armed when it starts (the pinned version's value; the kernel's `default_wall_time` for a run with no bot) | cancelled, ends `interrupted` with a `BudgetExceeded` diagnostic |
//! | `daily_token_ceiling` | at submit and at fire (typed refusal), in the claim (a queued run is passed over), and in the supervisor on each `Usage` | a run that crosses it is cancelled like a wall-time stop |
//!
//! Tokens are summed from `ExecEvent::Usage` (see its doc) per bot per local day, the day being
//! the one the `Clock`'s zone says. A day's row is never reset, so there is nothing to do at
//! midnight except wake the dispatcher: while a bot has queued runs held back by its ceiling, the
//! dispatcher arms one deadline at the next local midnight (ARCH-10).
//!
//! # The kill switch
//!
//! A pause is global or per bot, and it is persisted (`pauses`), so it outlives the daemon.
//!
//! - The dispatcher claims nothing for a paused scope; the runs stay `queued`.
//! - Live runs go on unless `stop_live` is given, which cancels them (each ends `interrupted`).
//! - A trigger of a paused bot is held: the scheduler arms no timer for it past the first due
//!   time, records one `trigger.held` event, and leaves its due time alone. On resume the due
//!   occurrences are caught up by the rule the scheduler already has for downtime: they merge
//!   into one run that says how late it is. A pause therefore loses nothing.
//! - A budget skip is the opposite: a trigger whose bot has used its ceiling records the
//!   occurrence as `trigger.skipped` (reason `budget_tokens`) and moves on to its next due time.
//!   It is not caught up, because the ceiling is there to stop that work, and it is not a failure
//!   to start, so it never backs the trigger off. The occurrence is an event rather than a
//!   firing because a firing points at a run and a skipped occurrence has none; `FiringOutcome`
//!   is unchanged on the wire.
//! - `trigger.fire` (a person asking) is not held by a pause: the run is queued and waits like any
//!   other. Under a spent ceiling it is refused with [`KernelError::BudgetExhausted`].

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use reno_proto::defs::{Budget, DefKind, parse_duration};
use reno_proto::{
    BotBudgetStatus, BudgetKind, BudgetPauseResult, BudgetResumeResult, BudgetStatusResult,
    BudgetStop,
};
use reno_store::GLOBAL_PAUSE;

use crate::defs;
use crate::error::{KernelError, Result};
use crate::shared::Shared;

/// What is paused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pauses {
    pub(crate) global: bool,
    pub(crate) bots: BTreeSet<String>,
}

impl Pauses {
    /// Whether runs and triggers of `bot` are held: everything is paused, or the bot is.
    pub(crate) fn holds(&self, bot: &str) -> bool {
        self.global || self.bots.contains(bot)
    }
}

/// The longest one run may take: `budget.max_wall_time`, or `fallback` when the text does not
/// parse (it was validated when stored, so that is a damaged row, not a user's mistake).
pub(crate) fn wall_time(budget: &Budget, fallback: Duration) -> Duration {
    parse_duration(&budget.max_wall_time).unwrap_or(fallback)
}

/// Loads the pauses the last daemon left.
pub(crate) async fn load(shared: &Shared) -> Result<()> {
    let mut pauses = Pauses::default();
    for scope in shared.db.pauses().await?.into_keys() {
        if scope == GLOBAL_PAUSE {
            pauses.global = true;
        } else {
            pauses.bots.insert(scope);
        }
    }
    shared.pauses_tx.send_replace(Arc::new(pauses));
    Ok(())
}

/// The name of the global pause is not a bot's: it would act on the wrong scope.
fn reject_reserved(bot: Option<&str>) -> Result<()> {
    match bot {
        Some(name) if name == GLOBAL_PAUSE => Err(KernelError::Invalid(format!(
            "{name:?} is not a bot name; leave the bot out to pause or resume everything"
        ))),
        _ => Ok(()),
    }
}

/// `budget.pause`.
pub(crate) async fn pause(
    shared: &Shared,
    bot: Option<&str>,
    stop_live: bool,
) -> Result<BudgetPauseResult> {
    if shared.root.is_cancelled() {
        return Err(KernelError::ShuttingDown);
    }
    reject_reserved(bot)?;
    if let Some(name) = bot {
        let known = shared.current_defs().set().bot(name).is_some()
            || shared.pauses_tx.borrow().bots.contains(name);
        if !known {
            return Err(defs::not_found(DefKind::Bot, name, None));
        }
    }
    let changed = shared.db.pause(bot.unwrap_or(GLOBAL_PAUSE)).await?;
    shared.pauses_tx.send_if_modified(|current| {
        let mut next = (**current).clone();
        match bot {
            Some(name) => next.bots.insert(name.to_owned()),
            None => !std::mem::replace(&mut next.global, true),
        };
        let modified = next != **current;
        if modified {
            *current = Arc::new(next);
        }
        modified
    });
    let mut stopped = Vec::new();
    if stop_live {
        for (run, cancel) in shared.live_of(bot) {
            cancel.cancel();
            stopped.push(run);
        }
        stopped.sort();
    }
    Ok(BudgetPauseResult { changed, stopped })
}

/// `budget.resume`.
pub(crate) async fn resume(shared: &Shared, bot: Option<&str>) -> Result<BudgetResumeResult> {
    if shared.root.is_cancelled() {
        return Err(KernelError::ShuttingDown);
    }
    reject_reserved(bot)?;
    let changed = shared.db.resume(bot.unwrap_or(GLOBAL_PAUSE)).await?;
    shared.pauses_tx.send_if_modified(|current| {
        let mut next = (**current).clone();
        match bot {
            Some(name) => next.bots.remove(name),
            None => std::mem::replace(&mut next.global, false),
        };
        let modified = next != **current;
        if modified {
            *current = Arc::new(next);
        }
        modified
    });
    // What was held back is claimable now; the scheduler hears the same change through the
    // watch and catches up its triggers.
    shared.wake.notify_one();
    Ok(BudgetResumeResult { changed })
}

/// The refusal a bot's spent ceiling earns a new run of it, or `None` when it has room (or no
/// ceiling). The ceiling is the one of the bot's definition in force now, which is the version a
/// new run is pinned to.
pub(crate) async fn token_refusal(shared: &Shared, bot: &str) -> Result<Option<BudgetStop>> {
    let Some(limit) = shared
        .current_defs()
        .set()
        .bot(bot)
        .and_then(|def| def.budget.daily_token_ceiling)
    else {
        return Ok(None);
    };
    let day = shared.db.today();
    let used = shared
        .db
        .bot_usage(&day)
        .await?
        .get(bot)
        .map_or(0, |tokens| tokens.total());
    Ok((used >= limit).then_some(BudgetStop {
        kind: BudgetKind::Tokens,
        limit,
        used,
    }))
}

/// `budget.status`.
pub(crate) async fn status(shared: &Shared) -> Result<BudgetStatusResult> {
    let now = shared.db.now_ms();
    let zone = shared.db.zone();
    let day = zone.day_of(now);
    let pauses = shared.pauses_tx.borrow().clone();
    let usage = shared.db.bot_usage(&day).await?;
    let occupancy = shared.db.occupancy().await?;
    let current = shared.current_defs();
    let ms = |wall: Duration| u64::try_from(wall.as_millis()).unwrap_or(u64::MAX);

    let mut names: BTreeSet<&str> = current.set().bots().keys().map(String::as_str).collect();
    names.extend(pauses.bots.iter().map(String::as_str));
    let fallback = Budget::default();
    let bots = names
        .into_iter()
        .map(|name| {
            let budget = current.set().bot(name).map_or(&fallback, |def| &def.budget);
            let busy = occupancy.get(name).copied().unwrap_or_default();
            BotBudgetStatus {
                bot: name.to_owned(),
                live_runs: busy.live,
                max_concurrent_runs: budget.max_concurrent_runs,
                queued_runs: busy.queued,
                tokens_today: usage.get(name).map_or(0, |tokens| tokens.total()),
                daily_token_ceiling: budget.daily_token_ceiling,
                max_wall_time_ms: ms(wall_time(budget, shared.config.default_wall_time)),
                paused: pauses.bots.contains(name),
            }
        })
        .collect();
    Ok(BudgetStatusResult {
        paused: pauses.global,
        day,
        day_ends_at_ms: u64::try_from(zone.next_day_start_ms(now)).unwrap_or(0),
        default_wall_time_ms: ms(shared.config.default_wall_time),
        bots,
    })
}
