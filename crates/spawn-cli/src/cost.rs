//! What the fleet's model calls cost (objective 041; docs/fleet-architecture.md
//! §3.5).
//!
//! The Bridge appends one `model.usage` event per assistant message it reads
//! from an execution's transcript (crate spawn-core, `ingest::claude_usage`).
//! The loop's cost step ([`record`]) folds them into `spawn_cost_messages`:
//! once per message id, its largest usage, at its earliest line's time,
//! under its model, for the execution, its agent, the task the agent held
//! then and that task's mission, and prices it there and then by
//! [`Prices`]: a later price change rewrites no past cost, and a restarted
//! loop counts no message twice. `GET /api/cost` ([`window`]) answers the
//! totals of the last 24 h or 7 d; the alerts step reads each budgeted
//! mission's spend of the UTC day (crate::alerts, `cost_budget`).
//!
//! Prices are `SPAWN_MODEL_PRICES`: a JSON object from model name to
//! `{"input", "output", "cache_write", "cache_read"}`, each US dollars per
//! million tokens, a number not below zero. Unset, [`BUILT_IN`] prices the
//! models the fleet runs; set, it replaces that table whole. A model with no
//! price counts its tokens and no cost (`usd` null: unpriced, never $0). A
//! value that is not such an object stops `spawn loop` and `spawn serve` at
//! start.

use serde_json::{Map, Value, json};
use spawn_postgres::PostgresStore;

use crate::config::env_value;

/// The setting's name.
pub(crate) const SETTING: &str = "SPAWN_MODEL_PRICES";

/// The four rates of a model, in the order a price names them.
const RATES: [&str; 4] = ["input", "output", "cache_write", "cache_read"];

/// The built-in prices, US dollars per million tokens (input, output, cache
/// write, cache read), Anthropic's first-party list prices as of 2026-11
/// (docs/fleet-architecture.md §3.5 names them). A model not here is
/// unpriced until SPAWN_MODEL_PRICES prices it.
const BUILT_IN: [(&str, [f64; 4]); 9] = [
    ("claude-fable-5-2", [8.0, 40.0, 10.0, 0.2]),
    ("claude-opus-5-5", [4.0, 20.0, 5.0, 0.2]),
    ("claude-sonnet-5-5", [2.0, 10.0, 2.5, 0.1]),
    ("claude-opus-4-8", [5.0, 25.0, 6.25, 0.5]),
    ("claude-opus-4-7", [5.0, 25.0, 6.25, 0.5]),
    ("claude-opus-4-6", [5.0, 25.0, 6.25, 0.5]),
    ("claude-sonnet-4-6", [3.0, 15.0, 3.75, 0.3]),
    ("claude-sonnet-4-5", [3.0, 15.0, 3.75, 0.3]),
    ("claude-haiku-4-5", [1.0, 5.0, 1.25, 0.1]),
];

/// The prices the cost step records messages at: the JSON object the store
/// takes (`{<model>: {input, output, cache_write, cache_read}}`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Prices(Value);

impl Prices {
    /// SPAWN_MODEL_PRICES, or the built-in table when it is unset or empty.
    pub(crate) fn from_env() -> Result<Self, String> {
        match env_value(SETTING) {
            Some(text) => Self::parse(&text),
            None => Ok(Self::built_in()),
        }
    }

    /// The built-in table ([`BUILT_IN`]).
    pub(crate) fn built_in() -> Self {
        let mut models = Map::new();
        for (model, rates) in BUILT_IN {
            let price: Map<String, Value> = RATES
                .iter()
                .zip(rates)
                .map(|(name, rate)| ((*name).to_string(), json!(rate)))
                .collect();
            models.insert(model.to_string(), Value::Object(price));
        }
        Self(Value::Object(models))
    }

    /// The setting's text, checked: a JSON object from model name to the
    /// four rates, each a number not below zero, and nothing else.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let bad = |why: String| format!("{SETTING}: {why}");
        let value: Value = serde_json::from_str(text)
            .map_err(|error| bad(format!("not JSON ({error})")))?;
        let Value::Object(models) = &value else {
            return Err(bad(
                "not a JSON object from model name to its rates".to_string(),
            ));
        };
        for (model, price) in models {
            if model.is_empty() {
                return Err(bad("a model has no name".to_string()));
            }
            let Value::Object(rates) = price else {
                return Err(bad(format!(
                    "{model}'s price is not an object of {}",
                    RATES.join(", ")
                )));
            };
            if let Some(unknown) = rates.keys().find(|key| !RATES.contains(&key.as_str())) {
                return Err(bad(format!(
                    "{model} has a rate {unknown}; the rates are {}",
                    RATES.join(", ")
                )));
            }
            for name in RATES {
                let rate = rates
                    .get(name)
                    .ok_or_else(|| bad(format!("{model} has no {name} rate")))?;
                match rate.as_f64() {
                    Some(number) if number.is_finite() && number >= 0.0 => {}
                    Some(_) => {
                        return Err(bad(format!(
                            "{model}'s {name} rate {rate} is below zero"
                        )));
                    }
                    None => {
                        return Err(bad(format!(
                            "{model}'s {name} rate {rate} is not a number"
                        )));
                    }
                }
            }
        }
        Ok(Self(value))
    }

    /// The object the store prices by.
    pub(crate) fn as_value(&self) -> &Value {
        &self.0
    }

    /// One message's cost at these prices, or `None` for a model they do
    /// not price (what `spawn_cost_usd` computes in the store).
    #[cfg(test)]
    fn usd(&self, model: &str, counts: [u64; 4]) -> Option<f64> {
        let rates = self.0.get(model)?;
        Some(
            RATES
                .iter()
                .zip(counts)
                .map(|(name, count)| {
                    #[expect(clippy::cast_precision_loss, reason = "a test's token counts")]
                    let count = count as f64;
                    count * rates[*name].as_f64().unwrap_or_default()
                })
                .sum::<f64>()
                / 1_000_000.0,
        )
    }
}

/// The loop's cost step: fold what the Bridge recorded since, priced now.
/// A line for the loop's log when it recorded anything.
pub(crate) fn record(store: &PostgresStore, prices: &Prices) -> Result<Option<String>, String> {
    let count = store
        .cost_record(prices.as_value())
        .map_err(|error| format!("cost: {error}"))?;
    Ok((count > 0).then(|| format!("cost: recorded {count} model call{}", plural(count))))
}

fn plural(count: i64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The windows `/api/cost` answers, and their length.
const WINDOWS: [(&str, i64); 2] = [("24h", 86_400_000), ("7d", 7 * 86_400_000)];

/// The window a request names (`24h` when it names none), and its length.
pub(crate) fn window_of(asked: Option<&str>) -> Result<(&'static str, i64), String> {
    let asked = asked.filter(|text| !text.is_empty()).unwrap_or("24h");
    WINDOWS
        .iter()
        .find(|(name, _)| *name == asked)
        .copied()
        .ok_or_else(|| format!("window is 24h or 7d, not {asked}"))
}

/// `GET /api/cost?window=`: the cost of the messages timed in [now minus
/// the window, now), on the database's clock.
pub(crate) fn window(store: &PostgresStore, name: &str, length_ms: i64) -> Result<Value, String> {
    let until_ms = store.database_now_ms().map_err(|error| error.to_string())?;
    let since_ms = until_ms.saturating_sub(length_ms);
    let mut answer = store
        .cost_window(since_ms, until_ms)
        .map_err(|error| error.to_string())?;
    if let Value::Object(map) = &mut answer {
        map.insert("window".to_string(), json!(name));
        map.insert("since_ms".to_string(), json!(since_ms));
        map.insert("until_ms".to_string(), json!(until_ms));
    }
    Ok(answer)
}

/// An amount in dollars as the alert's text says it: to the cent, and a
/// cost that is not zero never as $0.00.
pub(crate) fn dollars(usd: f64) -> String {
    let cents = (usd * 100.0).round() / 100.0;
    if usd > 0.0 && cents == 0.0 {
        "<$0.01".to_string()
    } else {
        format!("${cents:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::{Prices, SETTING, dollars, window_of};

    const TWO: &str = r#"{"bb-model-x":{"input":3,"output":15,"cache_write":3.75,"cache_read":0.3},
                          "bb-model-y":{"input":1,"output":5,"cache_write":1.25,"cache_read":0.1}}"#;

    #[test]
    fn a_message_costs_each_count_at_its_rate_and_an_unpriced_model_nothing() {
        let prices = Prices::parse(TWO).expect("valid prices");
        let x = prices
            .usd("bb-model-x", [1000, 2000, 10_000, 50_000])
            .expect("priced");
        assert!((x - 0.0855).abs() < 1e-12, "{x}");
        let y = prices.usd("bb-model-y", [200_000, 40_000, 0, 0]).expect("priced");
        assert!((y - 0.4).abs() < 1e-12, "{y}");
        assert_eq!(prices.usd("bb-model-z", [5000, 500, 0, 0]), None);
    }

    #[test]
    fn the_setting_is_an_object_of_four_rates_not_below_zero() {
        for bad in [
            "not json",
            "[]",
            r#"{"m": 3}"#,
            r#"{"m": {"input": 1, "output": 1, "cache_write": 1}}"#,
            r#"{"m": {"input": -1, "output": 1, "cache_write": 1, "cache_read": 1}}"#,
            r#"{"m": {"input": "1", "output": 1, "cache_write": 1, "cache_read": 1}}"#,
            r#"{"m": {"input": 1, "output": 1, "cache_write": 1, "cache_read": 1, "cache": 1}}"#,
            r#"{"": {"input": 1, "output": 1, "cache_write": 1, "cache_read": 1}}"#,
        ] {
            let error = Prices::parse(bad).expect_err(bad);
            assert!(error.contains(SETTING), "{bad}: {error}");
        }
        // Zero is a price (a free model), and an empty object prices nothing.
        Prices::parse(r#"{"m": {"input": 0, "output": 0, "cache_write": 0, "cache_read": 0}}"#)
            .expect("zero rates");
        Prices::parse("{}").expect("no prices");
    }

    #[test]
    fn the_built_in_table_passes_its_own_check() {
        let built_in = Prices::built_in();
        let text = built_in.as_value().to_string();
        assert_eq!(Prices::parse(&text).expect("valid"), built_in);
        assert!(built_in.usd("claude-opus-5-5", [1_000_000, 0, 0, 0]).is_some());
    }

    #[test]
    fn the_window_is_24h_by_default_or_7d_and_nothing_else() {
        assert_eq!(window_of(None), Ok(("24h", 86_400_000)));
        assert_eq!(window_of(Some("")), Ok(("24h", 86_400_000)));
        assert_eq!(window_of(Some("7d")), Ok(("7d", 604_800_000)));
        assert!(window_of(Some("30d")).is_err());
    }

    #[test]
    fn an_amount_reads_to_the_cent_and_never_zero_when_it_is_not() {
        assert_eq!(dollars(0.9355), "$0.94");
        assert_eq!(dollars(0.5), "$0.50");
        assert_eq!(dollars(0.001), "<$0.01");
        assert_eq!(dollars(0.0), "$0.00");
    }
}
