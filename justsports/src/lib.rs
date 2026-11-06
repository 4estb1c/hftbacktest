//! JustSports adapter for hftbacktest.
//!
//! Upstream hftbacktest stays as it is except for one addition, `GAP_EVENT`, which the
//! no-partial-fill exchange treats as a recording gap. Everything specific to JustSports lives
//! in this crate:
//!
//! - [`fees::ProbabilityFee`]: fees of the form `rate * p * (1 - p)` per contract (Kalshi);
//! - [`queue::JsQueue`]: the four queue settings of the join-late study (`front`,
//!   `back_trades`, `back_proportional`, `back_optimistic`), recording each order's queue at join;
//! - [`game`]: reads one game exported by `just_sports.sim.hft` (a `game.json` and one
//!   `.npz` event file per listing) and builds the backtest;
//! - [`join_late`]: the join-late strategy, a port of `just_sports.sim.maker.JoinLate`.
//!
//! Prices are exact: the exporter writes our integer ten-thousandths as `ticks * 0.0001`, and
//! the depth rounds `px / tick_size` back to the same integer tick.

pub mod fees;
pub mod game;
pub mod join_late;
pub mod queue;
