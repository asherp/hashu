//! The hashrate planner on the operator's dashboard, with epochs on (#126).
//!
//! A miner enters the TH/s they plan to run in each future difficulty
//! epoch (a build-out, rigs coming or going). Future epochs then sell
//! against the plan instead of the live average; the current epoch always
//! sells on what the rigs measure ([`hashu_core::epoch::plan`]).
//!
//! The plan is public, next to what each epoch delivered, because a plan
//! is a promise buyers price:
//!
//! - `GET /v1/hashu/plan`: the plan for the next epochs (with the
//!   capacity it gives each one) and, for every epoch this mint has run,
//!   the plan it started with and the work its hourly reliability record
//!   (#109) claims for it.
//! - Operator dashboard: `GET /api/plan` (the same) and
//!   `POST /api/plan` `{"epochs": {"<epoch>": <TH/s> | null}}` to set or
//!   clear future epochs. A plan freezes when its epoch starts.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use hashu_core::denom::TIH_PER_THH;
use hashu_core::epoch::start_height;
use hashu_mint::capacity::{now_ms, CapacityBook};
use hashu_mint::epoch::Epochs;
use hashu_mint::planner::HashratePlan;
use hashu_mint::reliability::RecordStore;
use serde_json::{json, Map, Value};

/// Future epochs the planner always lists, even beyond the horizon, so a
/// build-out can be entered before it comes into view.
pub const PLAN_AHEAD: u64 = 8;

/// The planner of one mint.
pub struct Planner {
    pub plan: Arc<HashratePlan>,
    pub epochs: Arc<Epochs>,
    pub capacity: Arc<CapacityBook>,
    pub record: Arc<RecordStore>,
}

impl Planner {
    /// Hook the plan into `epochs`' capacity and follow the chain.
    pub fn start(self: &Arc<Self>) {
        let plan = self.plan.clone();
        self.epochs
            .set_planned_hashrate(Arc::new(move |e| plan.planned(e)));
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                if let Some(c) = me.epochs.clock() {
                    me.plan.observe(&c, now_ms());
                }
            }
        });
    }

    /// The plan, its capacity per epoch, and planned vs delivered.
    pub fn view(&self) -> Result<Value, String> {
        let clock = self
            .epochs
            .clock()
            .ok_or("the epoch clock has not read the chain yet")?;
        let now = now_ms();
        let now_s = now / 1000;
        let current = clock.current();
        let rows = self.epochs.plan().unwrap_or_default();
        let last = self.epochs.last_epoch().unwrap_or(current);
        let future = self.plan.future();
        let through = last
            .max(current + PLAN_AHEAD)
            .max(future.keys().next_back().copied().unwrap_or(0))
            .min(current + hashu_mint::planner::MAX_EPOCHS_AHEAD);
        let snap = self.capacity.snapshot(now);
        let measured = snap.live.on.then_some(snap.live.stock.ewma_ths);

        let upcoming: Vec<Value> = (current..=through)
            .map(|e| {
                let row = rows.iter().find(|r| r.epoch == e);
                json!({
                    "epoch": e,
                    "current": e == current,
                    "start_height": start_height(e),
                    "starts_at": now_s + (clock.hours_until(e) * 3600.0) as u64,
                    "planned_ths": if e == current { None } else { future.get(&e).copied() },
                    "in_window": e <= last,
                    "hours_in_window": row.map(|r| r.hours_in_window),
                    "capacity_tih": row.map(|r| r.capacity_tih.floor() as u64),
                    "owed_tih": row.map(|r| r.owed_tih.ceil() as u64),
                    "free_tih": row.map(|r| r.free_tih.floor() as u64),
                })
            })
            .collect();

        let history: Vec<Value> = self
            .plan
            .history()
            .into_iter()
            .rev()
            .map(|(e, run)| {
                let start_s = run.start_ms / 1000;
                let end_ms = run.end_ms.unwrap_or(now);
                let (work, hours) = self.record.work_between(start_s, end_ms / 1000);
                let elapsed_h = end_ms.saturating_sub(run.start_ms) as f64 / 3_600_000.0;
                // Average over the time the epoch ran here, gaps included:
                // an hour missing from the record delivered nothing.
                let delivered_ths = (elapsed_h >= 1.0).then(|| work / TIH_PER_THH / elapsed_h);
                json!({
                    "epoch": e,
                    "current": e == current,
                    "planned_ths": run.planned_ths,
                    "started_at": start_s,
                    "ended_at": run.end_ms.map(|m| m / 1000),
                    "hours": elapsed_h,
                    "hours_published": hours,
                    "delivered_tih": work,
                    "delivered_ths": delivered_ths,
                    "planned_tih": run.planned_ths.map(|t| {
                        hashu_core::denom::tih_from_ths_hours(t, elapsed_h)
                    }),
                })
            })
            .collect();

        Ok(json!({
            "epoch": current,
            "horizon_hours": self.epochs.horizon_hours(),
            "last_epoch": last,
            "measured_ths": measured,
            "max_epoch": current + hashu_mint::planner::MAX_EPOCHS_AHEAD,
            "upcoming": upcoming,
            "history": history,
            "record": "/v1/hashu/reliability",
        }))
    }

    /// Apply `{"epochs": {"<epoch>": ths | null}}`. A cut that would leave
    /// an epoch owing more than its new capacity is refused: the TiH sold
    /// into it must still be deliverable.
    pub fn update(&self, body: &Value) -> Result<(), String> {
        let clock = self
            .epochs
            .clock()
            .ok_or("the epoch clock has not read the chain yet")?;
        let map = body
            .get("epochs")
            .and_then(Value::as_object)
            .ok_or("send {\"epochs\": {\"<epoch>\": <TH/s> | null}}")?;
        let changes = parse_changes(map)?;
        if changes.is_empty() {
            return Ok(());
        }
        let before = self.epochs.plan().unwrap_or_default();
        let undo: Vec<(u64, Option<f64>)> = changes
            .iter()
            .map(|(e, _)| (*e, self.plan.future().get(e).copied()))
            .collect();
        self.plan.set(&clock, &changes)?;
        let after = self.epochs.plan().unwrap_or_default();
        for (e, _) in &changes {
            let (Some(b), Some(a)) = (
                before.iter().find(|r| r.epoch == *e),
                after.iter().find(|r| r.epoch == *e),
            ) else {
                continue;
            };
            if a.capacity_tih < b.capacity_tih && a.owed_tih > a.capacity_tih + 0.5 {
                let _ = self.plan.set(&clock, &undo);
                return Err(format!(
                    "epoch {e} already owes {} TiH; that plan delivers only {} TiH in it",
                    a.owed_tih.ceil() as u64,
                    a.capacity_tih.floor() as u64
                ));
            }
        }
        tracing::info!(?changes, "hashrate plan updated");
        Ok(())
    }
}

fn parse_changes(map: &Map<String, Value>) -> Result<Vec<(u64, Option<f64>)>, String> {
    map.iter()
        .map(|(k, v)| {
            let e: u64 = k
                .trim()
                .parse()
                .map_err(|_| format!("{k:?} is not an epoch number"))?;
            let ths = match v {
                Value::Null => None,
                Value::Number(n) => n.as_f64(),
                _ => return Err(format!("epoch {e}: TH/s must be a number or null")),
            };
            Ok((e, ths))
        })
        .collect()
}

fn reply(r: Result<Value, String>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "detail": e })),
        )
            .into_response(),
    }
}

/// `GET /v1/hashu/plan` on the mint.
pub fn public_router(p: Arc<Planner>) -> axum::Router {
    axum::Router::new()
        .route("/v1/hashu/plan", get(get_plan))
        .with_state(p)
}

pub async fn get_plan(State(p): State<Arc<Planner>>) -> Response {
    reply(p.view())
}

pub async fn post_plan(State(p): State<Arc<Planner>>, body: String) -> Response {
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid JSON: {e}") })),
            )
                .into_response()
        }
    };
    match p.update(&v) {
        Ok(()) => reply(p.view()),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::denom::tih_from_ths_hours;
    use hashu_core::epoch::{EpochClock, Supply};
    use hashu_mint::epoch::PlanInput;

    /// Epoch 10 with 168 h left; 1 TH/s measured; `carried` TiH owed now.
    fn planner(dir: &std::path::Path, carried: f64) -> Planner {
        let epochs = Arc::new(Epochs::open(&[7u8; 32], vec![1, 2, 4], None).unwrap());
        epochs.set_clock(EpochClock::new(10 * 2016 + 1007));
        epochs.set_plan_inputs(Arc::new(move || PlanInput {
            supply: vec![Supply {
                ths: 1.0,
                hours_left: None,
            }],
            carried_tih: carried,
            horizon_hours: 168.0 + 2.0 * 336.0,
        }));
        let record = Arc::new(
            RecordStore::open(
                dir,
                "http://mint.test",
                hashu_mint::reliability::PublishConfig::parse("", ""),
            )
            .unwrap(),
        );
        let p = Planner {
            plan: Arc::new(HashratePlan::open(None).unwrap()),
            epochs,
            capacity: Arc::new(CapacityBook::new()),
            record,
        };
        let plan = p.plan.clone();
        p.epochs
            .set_planned_hashrate(Arc::new(move |e| plan.planned(e)));
        p
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hashu-planner-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn plan_sizes_future_epochs_and_is_public() {
        let d = tmp("sizes");
        let p = planner(&d, 0.0);
        p.update(&json!({"epochs": {"11": 5.0}})).unwrap();
        let v = p.view().unwrap();
        let up = v["upcoming"].as_array().unwrap();
        assert_eq!(up[0]["epoch"], 10);
        assert_eq!(up[1]["planned_ths"], 5.0);
        let cap = up[1]["capacity_tih"].as_u64().unwrap() as f64;
        assert!((cap - tih_from_ths_hours(5.0, 336.0)).abs() < 2.0, "{cap}");
        // Epoch 12 has no plan: it sells on the live average.
        let cap12 = up[2]["capacity_tih"].as_u64().unwrap() as f64;
        assert!((cap12 - tih_from_ths_hours(1.0, 336.0)).abs() < 2.0);
        // Listed beyond the horizon too, so a build-out can be planned.
        assert!(up.len() as u64 > PLAN_AHEAD);
        assert!(p.update(&json!({"epochs": {"10": 5.0}})).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cut_below_what_is_sold_is_refused() {
        let d = tmp("cut");
        // Owed now: all of epoch 10 (168 TH h) plus 100 TH h rolled into 11.
        let carried = tih_from_ths_hours(1.0, 168.0 + 100.0);
        let p = planner(&d, carried);
        // 0.1 TH/s × 336 h = 33.6 TH h < 100 owed in epoch 11.
        let e = p.update(&json!({"epochs": {"11": 0.1}})).unwrap_err();
        assert!(e.contains("epoch 11 already owes"), "{e}");
        assert_eq!(p.plan.planned(11), None);
        // Raising it is fine, and so is a cut that still covers it.
        p.update(&json!({"epochs": {"11": 3.0}})).unwrap();
        p.update(&json!({"epochs": {"11": 0.5}})).unwrap();
        assert_eq!(p.plan.planned(11), Some(0.5));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn parses_changes() {
        let v = json!({"12": 5.5, "13": null});
        let c = parse_changes(v.as_object().unwrap()).unwrap();
        assert_eq!(c, vec![(12, Some(5.5)), (13, None)]);
        assert!(parse_changes(json!({"x": 1}).as_object().unwrap()).is_err());
        assert!(parse_changes(json!({"12": "5"}).as_object().unwrap()).is_err());
    }
}
