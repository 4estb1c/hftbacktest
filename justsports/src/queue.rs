//! The queue settings of the join-late study, as an hftbacktest `QueueModel`.
//!
//! They follow `just_sports.sim.engine`: a resting order joins the back of its level (or the
//! front, for `Front`), trades at its price move it forward, and the rest of a level's drop is
//! cancels, which the setting places behind us (`BackTrades`), in proportion to the queue ahead
//! (`BackProportional`), or all ahead (`BackOptimistic`). Cancels never fill an order.
//!
//! As in upstream's `ProbQueueModel`, trade quantity seen at the level since its last depth
//! change is taken out of the next drop, so a trade and the book drop it causes count once.

use std::{
    any::Any,
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use hftbacktest::{
    backtest::models::QueueModel,
    depth::MarketDepth,
    types::{AnyClone, Order, Side},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Front,
    BackTrades,
    BackProportional,
    BackOptimistic,
}

impl Setting {
    pub const ALL: [Setting; 4] = [
        Setting::Front,
        Setting::BackTrades,
        Setting::BackProportional,
        Setting::BackOptimistic,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Setting::Front => "front",
            Setting::BackTrades => "back_trades",
            Setting::BackProportional => "back_proportional",
            Setting::BackOptimistic => "back_optimistic",
        }
    }

    pub fn parse(text: &str) -> Option<Setting> {
        Setting::ALL.into_iter().find(|s| s.name() == text)
    }
}

/// Queue state of one resting order.
#[derive(Clone, Debug, Default)]
pub struct JsPos {
    /// Contracts ahead of us.
    pub front: f64,
    /// Trade quantity at our price since the level last changed.
    pub cum_trade: f64,
}

impl AnyClone for JsPos {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Contracts ahead of each order when it came to rest, by order id.
pub type Joined = Arc<Mutex<HashMap<u64, f64>>>;

pub struct JsQueue<MD> {
    setting: Setting,
    joined: Joined,
    _md: PhantomData<MD>,
}

impl<MD> JsQueue<MD> {
    pub fn new(setting: Setting, joined: Joined) -> Self {
        Self {
            setting,
            joined,
            _md: PhantomData,
        }
    }
}

fn pos(order: &mut Order) -> &mut JsPos {
    order.q.as_any_mut().downcast_mut::<JsPos>().unwrap()
}

impl<MD: MarketDepth> QueueModel<MD> for JsQueue<MD> {
    fn new_order(&self, order: &mut Order, depth: &MD) {
        let level = if order.side == Side::Buy {
            depth.bid_qty_at_tick(order.price_tick)
        } else {
            depth.ask_qty_at_tick(order.price_tick)
        };
        let front = if self.setting == Setting::Front { 0.0 } else { level };
        self.joined.lock().unwrap().insert(order.order_id, front);
        order.q = Box::new(JsPos {
            front,
            cum_trade: 0.0,
        });
    }

    fn trade(&self, order: &mut Order, qty: f64, _depth: &MD) {
        let q = pos(order);
        q.front -= qty;
        q.cum_trade += qty;
    }

    fn depth(&self, order: &mut Order, prev_qty: f64, new_qty: f64, _depth: &MD) {
        let setting = self.setting;
        let q = pos(order);
        // A drop first spends trade quantity not yet seen in the book; only the rest is cancels.
        // An increase spends none, so a later decrement still pairs with its trade.
        let drop = prev_qty - new_qty;
        let traded = q.cum_trade.min(drop.max(0.0));
        q.cum_trade -= traded;
        let cancelled = drop - traded;
        if cancelled > 0.0 {
            match setting {
                Setting::Front | Setting::BackTrades => {}
                Setting::BackProportional => {
                    let level = prev_qty - traded;
                    if level > 0.0 {
                        q.front -= cancelled * q.front.max(0.0) / level;
                    }
                }
                Setting::BackOptimistic => q.front -= cancelled,
            }
        }
        // Only `back_trades` lets the queue ahead exceed the level, as `trades` does in sim.
        if setting != Setting::BackTrades {
            q.front = q.front.min(new_qty);
        }
        q.front = q.front.max(0.0);
    }

    fn is_filled(&self, order: &mut Order, depth: &MD) -> f64 {
        let q = pos(order);
        let exec = (-q.front / depth.lot_size()).round() as i64;
        if exec > 0 {
            q.front = 0.0;
            (exec as f64) * depth.lot_size()
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use hftbacktest::{
        depth::{HashMapMarketDepth, L2MarketDepth},
        types::{OrdType, TimeInForce},
    };

    use super::*;

    // a bid of one contract at 0.40 joining a level of 10
    fn setup(setting: Setting) -> (JsQueue<HashMapMarketDepth>, HashMapMarketDepth, Order) {
        let mut depth = HashMapMarketDepth::new(0.0001, 1e-6);
        depth.update_bid_depth(0.40, 10.0, 0);
        let mut order = Order::new(1, 4000, 0.0001, 1.0, Side::Buy, OrdType::Limit, TimeInForce::GTC);
        let joined: Joined = Default::default();
        let queue = JsQueue::new(setting, joined.clone());
        queue.new_order(&mut order, &depth);
        assert_eq!(joined.lock().unwrap()[&1], if setting == Setting::Front { 0.0 } else { 10.0 });
        (queue, depth, order)
    }

    fn front(order: &mut Order) -> f64 {
        pos(order).front
    }

    #[test]
    fn a_trade_and_its_drop_count_once() {
        for setting in [Setting::BackProportional, Setting::BackOptimistic] {
            let (q, depth, mut o) = setup(setting);
            q.trade(&mut o, 3.0, &depth);
            q.depth(&mut o, 10.0, 7.0, &depth); // the drop the trade caused: no cancels
            assert_eq!(front(&mut o), 7.0, "{setting:?}");
        }
    }

    #[test]
    fn an_increase_keeps_the_trade_for_its_drop() {
        let (q, depth, mut o) = setup(Setting::BackOptimistic);
        q.trade(&mut o, 3.0, &depth);
        q.depth(&mut o, 10.0, 12.0, &depth); // someone joins behind before the decrement
        q.depth(&mut o, 12.0, 9.0, &depth); // the trade's decrement
        assert_eq!(front(&mut o), 7.0);
    }

    #[test]
    fn cancels_by_setting() {
        // 4 contracts cancelled at a level of 10, all ahead of us at join
        let cases = [
            (Setting::BackTrades, 10.0),       // cancels sit behind us, no cap
            (Setting::BackProportional, 6.0),  // 4 * 10 / 10 ahead
            (Setting::BackOptimistic, 6.0),    // all 4 ahead
        ];
        for (setting, want) in cases {
            let (q, depth, mut o) = setup(setting);
            q.depth(&mut o, 10.0, 6.0, &depth);
            assert_eq!(front(&mut o), want, "{setting:?}");
        }
        // proportional splits by the share ahead: 10 ahead of a level of 20, 4 cancelled
        let (q, depth, mut o) = setup(Setting::BackProportional);
        q.depth(&mut o, 10.0, 20.0, &depth);
        q.depth(&mut o, 20.0, 16.0, &depth);
        assert_eq!(front(&mut o), 8.0);
    }

    #[test]
    fn trades_fill_only_past_the_queue() {
        let (q, depth, mut o) = setup(Setting::BackTrades);
        q.trade(&mut o, 10.0, &depth);
        assert_eq!(q.is_filled(&mut o, &depth), 0.0);
        q.trade(&mut o, 1.0, &depth);
        assert!(q.is_filled(&mut o, &depth) > 0.0);
        let (q, depth, mut o) = setup(Setting::Front);
        q.trade(&mut o, 1.0, &depth);
        assert!(q.is_filled(&mut o, &depth) > 0.0);
    }
}
