//! One game exported by `just_sports.sim.hft.export_game`, and the backtest built from it.
//!
//! The export folder holds `game.json` and one `<n>.npz` per listing. Each `.npz` is an
//! hftbacktest event array (key `data`): a book message becomes one row per level, a snapshot
//! a depth clear and its levels, a trade one row with its taker side, and a recording gap a
//! depth clear carrying `GAP_EVENT`. Every row carries both clocks: `exch_ts` is the venue
//! time and `local_ts` adds the feed latency.

use std::{
    fs,
    path::{Path, PathBuf},
};

use hftbacktest::{
    backtest::{
        Backtest,
        ExchangeKind,
        L2AssetBuilder,
        assettype::LinearAsset,
        data::{Data, DataSource, read_npz_file},
        models::ConstantLatency,
    },
    prelude::HashMapMarketDepth,
    types::Event,
};
use serde::Deserialize;

use crate::{
    fees::ProbabilityFee,
    queue::{JsQueue, Joined, Setting},
};

#[derive(Clone, Debug, Deserialize)]
pub struct ListingSpec {
    pub id: String,
    pub file: String,
    pub maker_rate: f64,
    pub taker_rate: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GameSpec {
    pub game: String,
    pub tick_size: f64,
    pub lot_size: f64,
    /// Decision to arrival at the venue, for orders and cancels.
    pub entry_ns: i64,
    /// Venue to our seeing it, for market data and our own order responses.
    pub feed_ns: i64,
    /// The last venue event time; the strategy sends nothing after it.
    pub end_ns: i64,
    pub listings: Vec<ListingSpec>,
}

pub struct Game {
    pub spec: GameSpec,
    pub data: Vec<Data<Event>>,
}

impl Game {
    pub fn load(dir: &Path) -> anyhow::Result<Game> {
        let text = fs::read_to_string(dir.join("game.json"))?;
        let spec: GameSpec = serde_json::from_str(&text)?;
        let mut data = Vec::with_capacity(spec.listings.len());
        for listing in &spec.listings {
            let path: PathBuf = dir.join(&listing.file);
            let events = read_npz_file::<Event>(path.to_str().unwrap(), "data")?;
            data.push(events);
        }
        Ok(Game { spec, data })
    }

    pub fn events(&self) -> usize {
        self.data.iter().map(Data::len).sum()
    }

    /// The fee model of each listing, in `spec.listings` order.
    pub fn fees(&self) -> Vec<ProbabilityFee> {
        self.spec
            .listings
            .iter()
            .map(|l| ProbabilityFee::new(l.maker_rate, l.taker_rate))
            .collect()
    }

    /// A backtest of the listings `assets` (indices into `spec.listings`, which become assets
    /// 0, 1, ...) under one queue setting. `joined` collects each order's queue ahead when it
    /// came to rest.
    ///
    /// hftbacktest finds the next event by scanning four clocks per asset, so a replay's cost
    /// per event grows with its assets. A strategy whose listings do not interact runs each
    /// listing as its own one-asset backtest.
    pub fn backtest(
        &self,
        setting: Setting,
        joined: &Joined,
        assets: &[usize],
    ) -> anyhow::Result<Backtest<HashMapMarketDepth>> {
        let mut builder = Backtest::builder();
        for &k in assets {
            let (listing, events) = (&self.spec.listings[k], &self.data[k]);
            let (tick, lot) = (self.spec.tick_size, self.spec.lot_size);
            let asset = L2AssetBuilder::new()
                .data(vec![DataSource::Data(events.clone())])
                .latency_model(ConstantLatency::new(self.spec.entry_ns, self.spec.feed_ns))
                .asset_type(LinearAsset::new(1.0))
                .fee_model(ProbabilityFee::new(listing.maker_rate, listing.taker_rate))
                .exchange(ExchangeKind::NoPartialFillExchange)
                .queue_model(JsQueue::new(setting, joined.clone()))
                .depth(move || HashMapMarketDepth::new(tick, lot))
                .build()?;
            builder = builder.add_asset(asset);
        }
        Ok(builder.build()?)
    }
}
