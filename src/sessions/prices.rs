//! Per-model USD rates from `~/.config/agents/model-prices.json` (override: `AGT_PRICES`).

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::core::error::{Error, Result};
use crate::core::local;
use crate::core::transcripts::json::{count, get, text};

pub struct Prices {
    models: Map<String, Value>,
}

/// USD for one response, split by what was billed.
#[derive(Clone, Copy, Default)]
pub struct Parts {
    pub input: f64,
    pub write: f64,
    pub read: f64,
    pub output: f64,
}

impl Parts {
    /// The parts in the order input, write, read, output.
    pub fn values(self) -> [f64; 4] {
        [self.input, self.write, self.read, self.output]
    }

    pub fn total(self) -> f64 {
        self.values().iter().fold(0.0, |sum, value| sum + value)
    }
}

fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGT_PRICES").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(local::home()?.join(".config/agents/model-prices.json"))
}

fn rate(rates: &Value, key: &str) -> f64 {
    get(rates, key).as_f64().unwrap_or_default()
}

impl Prices {
    pub fn load() -> Result<Self> {
        let path = path()?;
        let failed = |reason: String| {
            Error(format!(
                "cannot read prices from {}: {reason}",
                path.display()
            ))
        };
        let raw = std::fs::read_to_string(&path).map_err(|error| failed(error.to_string()))?;
        let value: Value = serde_json::from_str(&raw).map_err(|error| failed(error.to_string()))?;
        match value.get("models") {
            Some(Value::Object(models)) => Ok(Self {
                models: models.clone(),
            }),
            _ => Err(failed("no \"models\" object".into())),
        }
    }

    /// The rates of the longest model name that equals `model` or prefixes it before a `-`.
    fn rates(&self, model: &str) -> Option<&Value> {
        let mut best: Option<(&String, &Value)> = None;
        for (name, rates) in &self.models {
            let fits = model == name
                || model
                    .strip_prefix(name.as_str())
                    .is_some_and(|rest| rest.starts_with('-'));
            if fits && best.is_none_or(|(chosen, _)| name.len() > chosen.len()) {
                best = Some((name, rates));
            }
        }
        best.map(|(_, rates)| rates)
    }

    /// USD parts for one response, or `None` for a model the file does not price.
    pub fn usd(&self, model: &str, usage: &Value) -> Option<Parts> {
        let base = self.rates(model)?;
        let [input, written, read, output] = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "output_tokens",
        ]
        .map(|key| count(usage, key) as f64);
        let over = get(base, "over");
        let rates = if base.get("over").is_some()
            && input + written + read > get(over, "prompt_tokens").as_f64().unwrap_or_default()
        {
            over
        } else {
            base
        };
        let split = get(usage, "cache_creation");
        let (five, hour) = (
            get(split, "ephemeral_5m_input_tokens"),
            get(split, "ephemeral_1h_input_tokens"),
        );
        let (five, hour) = if five.is_null() && hour.is_null() {
            (written, 0.0)
        } else {
            (
                count(split, "ephemeral_5m_input_tokens") as f64,
                count(split, "ephemeral_1h_input_tokens") as f64,
            )
        };
        let multiplier = if text(usage, "speed") == "fast" {
            base.get("fast_multiplier")
                .and_then(Value::as_f64)
                .unwrap_or(1.0)
        } else {
            1.0
        };
        let scale = multiplier / 1e6;
        Some(Parts {
            input: input * rate(rates, "input") * scale,
            write: (five * rate(rates, "write_5m") + hour * rate(rates, "write_1h")) * scale,
            read: read * rate(rates, "read") * scale,
            output: output * rate(rates, "output") * scale,
        })
    }
}
