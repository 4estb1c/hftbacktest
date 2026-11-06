//! Run the join-late study on one exported game under each queue setting.
//!
//! Usage: join_late <game_dir> <out_dir> [--settings front,back_trades,...] [--settle-s 5]
//!        [--max-rest-s N] [--together]
//!
//! Listings do not interact in this strategy, so each runs as its own one-asset backtest.
//! `--together` runs the game as one multi-asset backtest instead, to time that path.
//!
//! Writes `orders_<setting>.csv` (one row per order, its fill if any) and `stats.json` (events,
//! seconds, and events per second for each setting) to `out_dir`.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use justsports::{
    fees::ProbabilityFee,
    game::Game,
    join_late::{JoinLate, OrderRecord, Params},
    queue::Setting,
};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        anyhow::bail!("usage: join_late <game_dir> <out_dir> [--settings a,b] [--settle-s 5]");
    }
    let game_dir = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);
    let mut settings: Vec<Setting> = Setting::ALL.to_vec();
    let mut params = Params::default();
    let mut together = false;
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--together" {
            together = true;
            i += 1;
            continue;
        }
        let value = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--settings" => {
                settings = value
                    .split(',')
                    .map(|s| Setting::parse(s).ok_or_else(|| anyhow::anyhow!("bad setting {s}")))
                    .collect::<anyhow::Result<_>>()?;
            }
            "--settle-s" => params.settle_ns = (value.parse::<f64>()? * 1e9) as i64,
            "--max-rest-s" => params.max_rest_ns = Some((value.parse::<f64>()? * 1e9) as i64),
            other => anyhow::bail!("unknown argument {other}"),
        }
        i += 2;
    }
    fs::create_dir_all(&out_dir)?;

    let t0 = Instant::now();
    let game = Game::load(&game_dir)?;
    let load_s = t0.elapsed().as_secs_f64();
    let events = game.events();
    let fees = game.fees();
    let n = fees.len();
    let groups: Vec<Vec<usize>> = if together {
        vec![(0..n).collect()]
    } else {
        (0..n).map(|k| vec![k]).collect()
    };

    let mut stats = Vec::new();
    for setting in settings {
        let t = Instant::now();
        let mut records: Vec<(usize, OrderRecord)> = Vec::new();
        for (g, assets) in groups.iter().enumerate() {
            let joined = Arc::new(Mutex::new(HashMap::new()));
            let mut hbt = game.backtest(setting, &joined, assets)?;
            let group_fees: Vec<ProbabilityFee> = assets.iter().map(|&k| fees[k].clone()).collect();
            let first = (g as u64) * 100_000_000 + 1;
            let mut strategy = JoinLate::new(
                params.clone(),
                game.spec.end_ns,
                game.spec.entry_ns,
                group_fees,
                joined,
                first,
            );
            strategy.run(&mut hbt)?;
            records.extend(strategy.orders.into_iter().map(|o| (assets[o.asset], o)));
        }
        let secs = t.elapsed().as_secs_f64();
        write_orders(&out_dir, &game, setting, &records)?;
        let filled = records.iter().filter(|(_, o)| o.status == "filled").count();
        println!(
            "{} {}: {} orders, {} filled, {} events in {:.2} s, {:.0} events/s",
            game.spec.game,
            setting.name(),
            records.len(),
            filled,
            events,
            secs,
            events as f64 / secs
        );
        stats.push(serde_json::json!({
            "setting": setting.name(),
            "orders": records.len(),
            "together": together,
            "filled": filled,
            "events": events,
            "seconds": secs,
            "events_per_s": events as f64 / secs,
        }));
    }
    let doc = serde_json::json!({"game": game.spec.game, "load_s": load_s, "runs": stats});
    fs::write(out_dir.join("stats.json"), serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn write_orders(
    out_dir: &std::path::Path,
    game: &Game,
    setting: Setting,
    records: &[(usize, OrderRecord)],
) -> anyhow::Result<()> {
    let path = out_dir.join(format!("orders_{}.csv", setting.name()));
    let mut f = std::io::BufWriter::new(fs::File::create(path)?);
    writeln!(
        f,
        "order_id,listing,side,price_tick,submit_ns,arrive_ns,joined_ahead,status,fill_ns,fill_tick,role,filled,fee"
    )?;
    let opt = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_default();
    for (listing, o) in records {
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            o.order_id,
            game.spec.listings[*listing].id,
            if o.bid { "bid" } else { "ask" },
            o.price_tick,
            o.submit_ns,
            o.arrive_ns,
            o.joined_ahead.map(|x| x.to_string()).unwrap_or_default(),
            o.status,
            opt(o.fill_ns),
            opt(o.fill_tick),
            o.maker.map(|m| if m { "maker" } else { "taker" }).unwrap_or(""),
            o.filled,
            o.fee.map(|x| x.to_string()).unwrap_or_default(),
        )?;
    }
    Ok(())
}
