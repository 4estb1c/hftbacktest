//! The join-late strategy, ported from `just_sports.sim.maker.JoinLate`.
//!
//! On each side of each listing it waits until the best price in our view has held
//! `settle_ns`, joins it with one contract, and rests until filled. It cancels when a better
//! price appears on its own side (or after `max_rest_ns`), and joins again under the same rule.
//!
//! Two differences from the Python engine follow from hftbacktest's order flow, not from the
//! strategy:
//! - hftbacktest cancels only an order the venue has acknowledged, so a cancel decided before
//!   the acknowledgement reaches us is sent when it does (one round trip later at most);
//! - our view wakes the strategy on any book row of a listing, and each book message is applied
//!   whole before the strategy runs, as in Python.

use std::{cmp::Reverse, collections::BinaryHeap};

use hftbacktest::{
    backtest::Backtest,
    depth::{INVALID_MAX, INVALID_MIN, MarketDepth},
    prelude::{Bot, ElapseResult, HashMapMarketDepth, OrdType, Status, TimeInForce},
};

use crate::{fees::ProbabilityFee, queue::Joined};

#[derive(Clone, Debug)]
pub struct Params {
    pub size: f64,
    pub settle_ns: i64,
    pub leave_when_behind: bool,
    pub max_rest_ns: Option<i64>,
    pub cooldown_ns: i64,
    pub recheck_ns: i64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            size: 1.0,
            settle_ns: 5_000_000_000,
            leave_when_behind: true,
            max_rest_ns: None,
            cooldown_ns: 0,
            recheck_ns: 100_000_000,
        }
    }
}

/// One order and what became of it. Prices are integer ticks.
#[derive(Clone, Debug)]
pub struct OrderRecord {
    pub order_id: u64,
    pub asset: usize,
    pub bid: bool,
    pub price_tick: i64,
    pub submit_ns: i64,
    pub arrive_ns: i64,
    pub joined_ahead: Option<f64>,
    pub status: &'static str,
    pub fill_ns: Option<i64>,
    pub fill_tick: Option<i64>,
    pub maker: Option<bool>,
    pub filled: f64,
    pub fee: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Timer {
    Join(usize),
    Expire(usize, u64),
    Recheck(usize, u64),
    Ready(usize),
}

#[derive(Clone, Debug, Default)]
struct Slot {
    best: Option<i64>,
    changed_ns: i64,
    order: Option<u64>,
    price: i64,
    cancelling: bool,
    cancel_sent: bool,
    ready_ns: i64,
    ready_pending: bool,
}

pub struct JoinLate {
    p: Params,
    end_ns: i64,
    entry_ns: i64,
    fees: Vec<ProbabilityFee>,
    joined: Joined,
    // slot k is asset k / 2, side bid when k is even
    slots: Vec<Slot>,
    timers: BinaryHeap<Reverse<(i64, u64, Timer)>>,
    seq: u64,
    next_order: u64,
    live: Vec<usize>, // indices into `orders` not yet known done
    pub orders: Vec<OrderRecord>,
    last_feed: Vec<i64>,
}

fn is_done(status: Status) -> bool {
    matches!(status, Status::Filled | Status::Canceled | Status::Expired)
}

impl JoinLate {
    /// `fees` has one entry per asset of the backtest; order ids start at `first_order_id`.
    pub fn new(
        p: Params,
        end_ns: i64,
        entry_ns: i64,
        fees: Vec<ProbabilityFee>,
        joined: Joined,
        first_order_id: u64,
    ) -> Self {
        let n = fees.len();
        Self {
            p,
            end_ns,
            entry_ns,
            fees,
            joined,
            slots: vec![Slot::default(); 2 * n],
            timers: BinaryHeap::new(),
            seq: 0,
            next_order: first_order_id,
            live: Vec::new(),
            orders: Vec::new(),
            last_feed: vec![i64::MIN; n],
        }
    }

    fn schedule(&mut self, at: i64, timer: Timer) {
        self.seq += 1;
        self.timers.push(Reverse((at, self.seq, timer)));
    }

    /// Run the whole replay.
    pub fn run(&mut self, hbt: &mut Backtest<HashMapMarketDepth>) -> anyhow::Result<()> {
        let n = self.fees.len();
        loop {
            let now = hbt.current_timestamp();
            // timers due now (cur_ts is i64::MAX before the first step)
            if now != i64::MAX {
                while let Some(Reverse((at, _, _))) = self.timers.peek() {
                    if *at > now {
                        break;
                    }
                    let Reverse((_, _, timer)) = self.timers.pop().unwrap();
                    self.on_timer(hbt, now, timer)?;
                }
            }
            let timeout = match (self.timers.peek(), now) {
                (_, i64::MAX) => 1,
                (Some(Reverse((at, _, _))), _) => *at - now,
                (None, _) => 3_600_000_000_000,
            };
            if let ElapseResult::EndOfData = hbt.wait_next_feed(true, timeout)? {
                // the last step can carry order responses, such as a fill on the final trade
                let now = hbt.current_timestamp();
                self.responses(hbt, now)?;
                break;
            }
            let now = hbt.current_timestamp();
            self.responses(hbt, now)?;
            for asset in 0..n {
                if let Some((_, local)) = hbt.feed_latency(asset) {
                    if local == now && self.last_feed[asset] != now {
                        self.last_feed[asset] = now;
                        self.on_event(hbt, now, asset)?;
                    }
                }
            }
        }
        // orders still open at the end
        for &k in &self.live {
            self.orders[k].status = "open";
        }
        Ok(())
    }

    /// Record what the venue told us since the last step; a fill wakes its side.
    fn responses(&mut self, hbt: &mut Backtest<HashMapMarketDepth>, now: i64) -> anyhow::Result<()> {
        let mut filled_slots = Vec::new();
        let mut keep = Vec::with_capacity(self.live.len());
        for &k in &self.live {
            let rec = &mut self.orders[k];
            let Some(order) = hbt.orders(rec.asset).get(&rec.order_id) else {
                keep.push(k);
                continue;
            };
            if !is_done(order.status) {
                keep.push(k);
                continue;
            }
            rec.joined_ahead = self.joined.lock().unwrap().get(&rec.order_id).copied();
            match order.status {
                Status::Filled => {
                    rec.status = "filled";
                    rec.fill_ns = Some(order.exch_timestamp);
                    rec.fill_tick = Some(order.exec_price_tick);
                    rec.maker = Some(order.maker);
                    rec.filled = order.exec_qty;
                    let price = order.exec_price_tick as f64 * order.tick_size;
                    rec.fee = Some(
                        self.fees[rec.asset].per_contract(price, order.maker) * order.exec_qty,
                    );
                    filled_slots.push(2 * rec.asset + usize::from(!rec.bid));
                }
                Status::Canceled => rec.status = "cancelled",
                _ => rec.status = "censored",
            }
        }
        self.live = keep;
        for slot in filled_slots {
            self.refresh(hbt, now, slot);
            self.try_join(hbt, now, slot)?;
        }
        // cancels decided before the venue acknowledged the order go out once it has
        for slot in 0..self.slots.len() {
            let s = &self.slots[slot];
            if s.cancelling && !s.cancel_sent {
                if let Some(id) = s.order {
                    let asset = slot / 2;
                    if let Some(order) = hbt.orders(asset).get(&id) {
                        if order.cancellable() {
                            hbt.cancel(asset, id, false)?;
                            self.slots[slot].cancel_sent = true;
                        } else if is_done(order.status) {
                            self.slots[slot].cancel_sent = true;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn ended(&self, now: i64) -> bool {
        now >= self.end_ns
    }

    fn view_best(depth: &HashMapMarketDepth, bid: bool) -> Option<i64> {
        if bid {
            let t = depth.best_bid_tick();
            (t != INVALID_MIN).then_some(t)
        } else {
            let t = depth.best_ask_tick();
            (t != INVALID_MAX).then_some(t)
        }
    }

    fn on_event(
        &mut self,
        hbt: &mut Backtest<HashMapMarketDepth>,
        now: i64,
        asset: usize,
    ) -> anyhow::Result<()> {
        for bid in [true, false] {
            let slot = 2 * asset + usize::from(!bid);
            let best = Self::view_best(hbt.depth(asset), bid);
            if best != self.slots[slot].best {
                self.slots[slot].best = best;
                self.slots[slot].changed_ns = now;
                if best.is_some() && self.p.settle_ns > 0 {
                    self.schedule(now + self.p.settle_ns, Timer::Join(slot));
                }
            }
            self.refresh(hbt, now, slot);
            let s = &self.slots[slot];
            if let (Some(_), Some(b)) = (s.order, best) {
                let better = if bid { b > s.price } else { b < s.price };
                if self.p.leave_when_behind && !s.cancelling && better {
                    self.cancel(hbt, now, slot)?;
                }
            }
            self.try_join(hbt, now, slot)?;
        }
        Ok(())
    }

    fn on_timer(
        &mut self,
        hbt: &mut Backtest<HashMapMarketDepth>,
        now: i64,
        timer: Timer,
    ) -> anyhow::Result<()> {
        let slot = match timer {
            Timer::Join(s) | Timer::Expire(s, _) | Timer::Recheck(s, _) | Timer::Ready(s) => s,
        };
        let mine = |s: &Slot, id: u64| s.order == Some(id);
        match timer {
            Timer::Expire(_, id) => {
                let s = &self.slots[slot];
                if mine(s, id) && !s.cancelling && !self.order_done(hbt, slot, id) {
                    self.cancel(hbt, now, slot)?;
                }
            }
            Timer::Ready(_) => self.slots[slot].ready_pending = false,
            _ => {}
        }
        self.refresh(hbt, now, slot);
        if let Timer::Recheck(_, id) = timer {
            if mine(&self.slots[slot], id) && !self.ended(now) {
                self.schedule(now + self.p.recheck_ns, timer);
            }
        }
        self.try_join(hbt, now, slot)
    }

    fn order_done(&self, hbt: &Backtest<HashMapMarketDepth>, slot: usize, id: u64) -> bool {
        hbt.orders(slot / 2)
            .get(&id)
            .map(|o| is_done(o.status))
            .unwrap_or(false)
    }

    fn cancel(
        &mut self,
        hbt: &mut Backtest<HashMapMarketDepth>,
        now: i64,
        slot: usize,
    ) -> anyhow::Result<()> {
        let id = self.slots[slot].order.unwrap();
        let asset = slot / 2;
        self.slots[slot].cancelling = true;
        self.slots[slot].cancel_sent = false;
        if let Some(order) = hbt.orders(asset).get(&id) {
            if order.cancellable() {
                hbt.cancel(asset, id, false)?;
                self.slots[slot].cancel_sent = true;
            }
        }
        self.schedule(now + self.p.recheck_ns, Timer::Recheck(slot, id));
        Ok(())
    }

    /// Free the slot once its order is known done; start the cooldown.
    fn refresh(&mut self, hbt: &Backtest<HashMapMarketDepth>, now: i64, slot: usize) {
        if let Some(id) = self.slots[slot].order {
            if self.order_done(hbt, slot, id) {
                let s = &mut self.slots[slot];
                s.order = None;
                s.cancelling = false;
                s.ready_ns = now + self.p.cooldown_ns;
            }
        }
    }

    fn try_join(
        &mut self,
        hbt: &mut Backtest<HashMapMarketDepth>,
        now: i64,
        slot: usize,
    ) -> anyhow::Result<()> {
        let s = &self.slots[slot];
        let Some(best) = s.best else { return Ok(()) };
        if s.order.is_some() || self.ended(now) || now < s.changed_ns + self.p.settle_ns {
            return Ok(());
        }
        if now < s.ready_ns {
            if !s.ready_pending {
                let at = s.ready_ns;
                self.slots[slot].ready_pending = true;
                self.schedule(at, Timer::Ready(slot));
            }
            return Ok(());
        }
        let asset = slot / 2;
        let bid = slot % 2 == 0;
        let depth = hbt.depth(asset);
        let (b, a) = (Self::view_best(depth, true), Self::view_best(depth, false));
        let (Some(b), Some(a)) = (b, a) else { return Ok(()) };
        if b >= a {
            return Ok(()); // crossed or locked: no mid
        }
        let tick = depth.tick_size();
        let id = self.next_order;
        self.next_order += 1;
        let price = best as f64 * tick;
        if bid {
            hbt.submit_buy_order(asset, id, price, self.p.size, TimeInForce::GTC, OrdType::Limit, false)?;
        } else {
            hbt.submit_sell_order(asset, id, price, self.p.size, TimeInForce::GTC, OrdType::Limit, false)?;
        }
        self.live.push(self.orders.len());
        self.orders.push(OrderRecord {
            order_id: id,
            asset,
            bid,
            price_tick: best,
            submit_ns: now,
            arrive_ns: now + self.entry_ns,
            joined_ahead: None,
            status: "open",
            fill_ns: None,
            fill_tick: None,
            maker: None,
            filled: 0.0,
            fee: None,
        });
        let s = &mut self.slots[slot];
        s.order = Some(id);
        s.price = best;
        s.cancelling = false;
        s.cancel_sent = false;
        if let Some(rest) = self.p.max_rest_ns {
            self.schedule(now + rest, Timer::Expire(slot, id));
        }
        Ok(())
    }
}
