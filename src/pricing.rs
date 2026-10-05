//! Token prices per harness, used by the COST screen to turn usage into a
//! cost.
//!
//! The built-in tables follow the official price pages on the date in
//! `*_PRICES_UPDATED`. Prices are USD per million tokens. `config.json` can
//! add models or override them under `pricing.<harness>`, so a price change
//! does not need a release.
//!
//! - Claude: Anthropic API list prices (platform.claude.com pricing page).
//!   Cache writes are billed by TTL, cache reads at a model-specific rate.
//!   Subscription usage is not billed per token, so this is an
//!   API-equivalent cost.
//! - Codex: OpenAI API prices (developers.openai.com pricing page). The
//!   Codex credit rate card lists the same rates in credits at
//!   `CODEX_USD_PER_CREDIT`. OpenAI does not bill cache writes; `input` is
//!   the uncached part of the prompt and `cache_read` the cached part. The
//!   standard (short-context) rates are used; long-context and priority
//!   requests cost more and cannot be told apart in the logs.

use crate::harness::Harness;
use crate::usage::TokenBreakdown;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Date the built-in Claude table was checked against the official page.
pub const CLAUDE_PRICES_UPDATED: &str = "2026-10-05";
/// Date the built-in Codex table was checked against the official pages.
pub const CODEX_PRICES_UPDATED: &str = "2026-10-05";
/// USD value of one Codex credit: the credit rate card is the API price
/// table divided by this.
pub const CODEX_USD_PER_CREDIT: f64 = 0.04;

/// Prices of one model in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// Uncached input.
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

impl ModelPrice {
    /// Anthropic's standard multipliers: five-minute writes 1.25x input,
    /// one-hour writes 2x input, cache reads 0.1x input.
    fn claude(input: f64, output: f64) -> Self {
        Self {
            input,
            cache_write_5m: input * 1.25,
            cache_write_1h: input * 2.0,
            cache_read: input * 0.1,
            output,
        }
    }

    fn with_cache_read(self, cache_read: f64) -> Self {
        Self { cache_read, ..self }
    }

    /// OpenAI prices: input, cached input, and output; no cache writes.
    fn openai(input: f64, cached_input: f64, output: f64) -> Self {
        Self {
            input,
            cache_write_5m: 0.0,
            cache_write_1h: 0.0,
            cache_read: cached_input,
            output,
        }
    }
}

/// A price from `config.json`. Cache prices that are left out follow
/// Anthropic's standard multipliers of `input`; Codex logs have no cache
/// writes, so only `cache_read` matters there.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceOverride {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
}

impl PriceOverride {
    fn price(self) -> ModelPrice {
        let standard = ModelPrice::claude(self.input, self.output);
        ModelPrice {
            cache_write_5m: self.cache_write_5m.unwrap_or(standard.cache_write_5m),
            cache_write_1h: self.cache_write_1h.unwrap_or(standard.cache_write_1h),
            cache_read: self.cache_read.unwrap_or(standard.cache_read),
            ..standard
        }
    }
}

/// The `pricing` section of `config.json`: model id -> price, per harness.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingOverrides {
    pub codex: BTreeMap<String, PriceOverride>,
    pub claude: BTreeMap<String, PriceOverride>,
}

/// Cost in USD split by token category.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostBreakdown {
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
}

impl CostBreakdown {
    pub fn total(&self) -> f64 {
        self.input + self.cache_write + self.cache_read + self.output
    }

    pub fn add(&mut self, other: CostBreakdown) {
        self.input += other.input;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
        self.output += other.output;
    }
}

/// The table entry a model id resolved to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceMatch<'a> {
    pub price: &'a ModelPrice,
    /// The table id, which differs from the model id when the model is
    /// priced by its date-less id or its family.
    pub id: &'a str,
    /// Priced by family prefix rather than its own id.
    pub by_family: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceTable {
    prices: BTreeMap<String, ModelPrice>,
    /// Date of the built-in table.
    pub updated: &'static str,
    /// Number of models set in `config.json`.
    pub overrides: usize,
}

impl PriceTable {
    pub fn built_in(harness: Harness) -> Self {
        let (prices, updated) = match harness {
            Harness::Codex => (codex_prices(), CODEX_PRICES_UPDATED),
            Harness::Claude => (claude_prices(), CLAUDE_PRICES_UPDATED),
        };
        Self {
            prices: prices
                .into_iter()
                .map(|(model, price)| (model.to_string(), price))
                .collect(),
            updated,
            overrides: 0,
        }
    }

    fn with_overrides(mut self, overrides: &BTreeMap<String, PriceOverride>) -> Self {
        for (model, price) in overrides {
            self.prices.insert(model.clone(), price.price());
        }
        self.overrides = overrides.len();
        self
    }

    /// Price of a model id: the exact id, then the id without a date suffix,
    /// then the longest table id it continues with a `-` (its family, for
    /// example `gpt-5.1` for `gpt-5.1-codex-max`).
    pub fn price_for(&self, model: &str) -> Option<PriceMatch<'_>> {
        let undated = strip_date_suffix(model);
        for id in [model, undated] {
            if let Some((id, price)) = self.prices.get_key_value(id) {
                return Some(PriceMatch {
                    price,
                    id,
                    by_family: false,
                });
            }
        }
        self.prices
            .iter()
            .filter(|(id, _)| {
                undated
                    .strip_prefix(id.as_str())
                    .is_some_and(|rest| rest.starts_with('-'))
            })
            .max_by_key(|(id, _)| id.len())
            .map(|(id, price)| PriceMatch {
                price,
                id,
                by_family: true,
            })
    }

    /// Cost of `tokens` for `model`, or `None` when the model has no price.
    pub fn cost(&self, model: &str, tokens: TokenBreakdown) -> Option<CostBreakdown> {
        let price = self.price_for(model)?.price;
        let per_token = |count: i64, usd_per_million: f64| count as f64 * usd_per_million / 1e6;
        let one_hour = tokens.cache_write_1h.clamp(0, tokens.cache_write);
        Some(CostBreakdown {
            input: per_token(tokens.input, price.input),
            cache_write: per_token(tokens.cache_write - one_hour, price.cache_write_5m)
                + per_token(one_hour, price.cache_write_1h),
            cache_read: per_token(tokens.cache_read, price.cache_read),
            output: per_token(tokens.output, price.output),
        })
    }
}

/// The price tables of every harness.
#[derive(Debug, Clone, PartialEq)]
pub struct Pricing {
    pub codex: PriceTable,
    pub claude: PriceTable,
}

impl Default for Pricing {
    fn default() -> Self {
        Self::new(&PricingOverrides::default())
    }
}

impl Pricing {
    pub fn new(overrides: &PricingOverrides) -> Self {
        Self {
            codex: PriceTable::built_in(Harness::Codex).with_overrides(&overrides.codex),
            claude: PriceTable::built_in(Harness::Claude).with_overrides(&overrides.claude),
        }
    }

    pub fn table(&self, harness: Harness) -> &PriceTable {
        match harness {
            Harness::Codex => &self.codex,
            Harness::Claude => &self.claude,
        }
    }
}

/// Anthropic API list prices.
fn claude_prices() -> Vec<(&'static str, ModelPrice)> {
    let opus_4 = ModelPrice::claude(15.0, 75.0);
    let opus_4_5 = ModelPrice::claude(5.0, 25.0);
    let sonnet_4 = ModelPrice::claude(3.0, 15.0);
    let fable_5_1 = ModelPrice::claude(10.0, 50.0).with_cache_read(0.25);
    vec![
        ("claude-fable-5-1", fable_5_1),
        ("claude-mythos-5-1", fable_5_1),
        ("claude-fable-5", ModelPrice::claude(10.0, 50.0)),
        ("claude-mythos-5", ModelPrice::claude(10.0, 50.0)),
        (
            "claude-opus-5-5",
            ModelPrice::claude(4.0, 20.0).with_cache_read(0.20),
        ),
        ("claude-opus-5", opus_4_5),
        ("claude-opus-4-8", opus_4_5),
        ("claude-opus-4-7", opus_4_5),
        ("claude-opus-4-6", opus_4_5),
        ("claude-opus-4-5", opus_4_5),
        ("claude-opus-4-1", opus_4),
        ("claude-opus-4", opus_4),
        ("claude-sonnet-5-5", ModelPrice::claude(2.0, 10.0)),
        ("claude-sonnet-5", ModelPrice::claude(2.0, 10.0)),
        ("claude-sonnet-4-6", sonnet_4),
        ("claude-sonnet-4-5", sonnet_4),
        ("claude-sonnet-4", sonnet_4),
        ("claude-haiku-4-5", ModelPrice::claude(1.0, 5.0)),
        ("claude-3-5-haiku", ModelPrice::claude(0.80, 4.0)),
    ]
}

/// OpenAI API prices (input, cached input, output).
fn codex_prices() -> Vec<(&'static str, ModelPrice)> {
    vec![
        ("gpt-6-astra", ModelPrice::openai(10.0, 1.0, 50.0)),
        ("gpt-6.1-sol", ModelPrice::openai(2.0, 0.10, 10.0)),
        ("gpt-6-sol", ModelPrice::openai(2.0, 0.20, 10.0)),
        ("gpt-6-luna", ModelPrice::openai(0.10, 0.01, 0.50)),
        ("gpt-5.6-sol", ModelPrice::openai(4.0, 0.40, 20.0)),
        ("gpt-5.6-terra", ModelPrice::openai(2.0, 0.20, 12.0)),
        ("gpt-5.6-luna", ModelPrice::openai(0.20, 0.02, 1.20)),
        ("gpt-5.5", ModelPrice::openai(5.0, 0.50, 30.0)),
        ("gpt-5.4", ModelPrice::openai(2.50, 0.25, 15.0)),
        ("gpt-5.4-mini", ModelPrice::openai(0.75, 0.075, 4.50)),
        ("gpt-5.3-codex", ModelPrice::openai(1.75, 0.175, 14.0)),
        ("gpt-5.2", ModelPrice::openai(1.75, 0.175, 14.0)),
        ("gpt-5.1", ModelPrice::openai(1.25, 0.125, 10.0)),
        ("gpt-5", ModelPrice::openai(1.25, 0.125, 10.0)),
        ("gpt-5-mini", ModelPrice::openai(0.25, 0.025, 2.0)),
        // "Daybreak Blue" on the Codex credit rate card (100 / 10 / 500
        // credits); Codex logs it as gpt-daybreak-blue-latest.
        ("gpt-daybreak-blue", ModelPrice::openai(4.0, 0.40, 20.0)),
    ]
}

/// Drops a trailing date (`-YYYYMMDD` or `-YYYY-MM-DD`) from a model id.
fn strip_date_suffix(model: &str) -> &str {
    let is_digits =
        |text: &str, len: usize| text.len() == len && text.bytes().all(|b| b.is_ascii_digit());
    if let Some((head, tail)) = model.rsplit_once('-') {
        if is_digits(tail, 8) {
            return head;
        }
        // -YYYY-MM-DD
        let parts: Vec<&str> = model.rsplitn(4, '-').collect();
        if let [day, month, year, head] = parts[..] {
            if is_digits(year, 4) && is_digits(month, 2) && is_digits(day, 2) {
                return head;
            }
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(
        input: i64,
        cache_write: i64,
        cache_write_1h: i64,
        cache_read: i64,
        output: i64,
    ) -> TokenBreakdown {
        TokenBreakdown {
            input,
            cache_write,
            cache_write_1h,
            cache_read,
            output,
        }
    }

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn claude_prices_apply_ttl_specific_cache_write_rates() {
        let table = PriceTable::built_in(Harness::Claude);
        // 1M input ($4) + 1M 5-minute writes ($5) + 1M 1-hour writes ($8)
        // + 10M cache reads ($2) + 100k output ($2).
        let opus = table
            .cost(
                "claude-opus-5-5",
                tokens(1_000_000, 2_000_000, 1_000_000, 10_000_000, 100_000),
            )
            .expect("opus price");
        assert!(close(opus.total(), 21.0), "{opus:?}");
        // 1M input ($1) + 1M 5-minute writes ($1.25) + 10M reads ($1) + 200k output ($1).
        let haiku = table
            .cost(
                "claude-haiku-4-5-20251001",
                tokens(1_000_000, 1_000_000, 0, 10_000_000, 200_000),
            )
            .expect("haiku price");
        assert!(close(haiku.total(), 4.25), "{haiku:?}");
        let fable = table.price_for("claude-fable-5-1").expect("fable");
        assert!(close(fable.price.cache_read, 0.25));
        assert!(close(fable.price.cache_write_1h, 20.0));
    }

    #[test]
    fn codex_prices_split_cached_input_and_match_the_credit_card() {
        let table = PriceTable::built_in(Harness::Codex);
        // 1M uncached ($5) + 10M cached ($5) + 100k output ($3).
        let cost = table
            .cost("gpt-5.5", tokens(1_000_000, 0, 0, 10_000_000, 100_000))
            .expect("gpt-5.5 price");
        assert!(close(cost.total(), 13.0), "{cost:?}");
        // The credit rate card lists GPT-5.5 at 125 / 12.5 / 750 credits.
        let price = table.price_for("gpt-5.5").expect("gpt-5.5").price;
        assert!(close(price.input / CODEX_USD_PER_CREDIT, 125.0));
        assert!(close(price.cache_read / CODEX_USD_PER_CREDIT, 12.5));
        assert!(close(price.output / CODEX_USD_PER_CREDIT, 750.0));
    }

    #[test]
    fn price_lookup_handles_dates_families_and_unknown_models() {
        let claude = PriceTable::built_in(Harness::Claude);
        let haiku = claude
            .price_for("claude-haiku-4-5-20251001")
            .expect("haiku");
        assert_eq!((haiku.id, haiku.by_family), ("claude-haiku-4-5", false));
        let next = claude.price_for("claude-opus-5-9").expect("family");
        assert_eq!((next.id, next.by_family), ("claude-opus-5", true));
        assert!(claude.price_for("gpt-test").is_none());

        let codex = PriceTable::built_in(Harness::Codex);
        let max = codex.price_for("gpt-5.1-codex-max").expect("family");
        assert_eq!((max.id, max.by_family), ("gpt-5.1", true));
        let blue = codex.price_for("gpt-daybreak-blue-latest").expect("family");
        assert_eq!(blue.id, "gpt-daybreak-blue");
        let dated = codex.price_for("gpt-5-2025-08-07").expect("dated");
        assert_eq!((dated.id, dated.by_family), ("gpt-5", false));
        // A family prefix must end at a dash: gpt-5 does not price gpt-5.9.
        assert!(codex.price_for("gpt-5.9-nova").is_none());
        assert!(codex.price_for("codex-auto-review").is_none());
        assert!(codex
            .cost("codex-auto-review", tokens(1, 0, 0, 0, 1))
            .is_none());
    }

    #[test]
    fn config_overrides_add_models_and_fill_cache_prices() {
        let mut overrides = PricingOverrides::default();
        overrides.codex.insert(
            "codex-auto-review".to_string(),
            PriceOverride {
                input: 1.0,
                output: 4.0,
                cache_read: Some(0.05),
                cache_write_5m: None,
                cache_write_1h: None,
            },
        );
        overrides.claude.insert(
            "claude-opus-5-5".to_string(),
            PriceOverride {
                input: 2.0,
                output: 10.0,
                cache_read: None,
                cache_write_5m: None,
                cache_write_1h: None,
            },
        );
        let pricing = Pricing::new(&overrides);
        assert_eq!(pricing.codex.overrides, 1);
        let review = pricing.codex.price_for("codex-auto-review").expect("added");
        assert!(close(review.price.cache_read, 0.05));
        let opus = pricing
            .claude
            .price_for("claude-opus-5-5")
            .expect("override");
        assert!(
            close(opus.price.cache_read, 0.2),
            "0.1x input when left out"
        );
        assert!(close(opus.price.cache_write_1h, 4.0));

        let parsed: PricingOverrides =
            serde_json::from_str(r#"{"claude": {"claude-test-1": {"input": 3, "output": 15}}}"#)
                .expect("parse overrides");
        assert!(parsed.codex.is_empty());
        assert!(close(parsed.claude["claude-test-1"].input, 3.0));
    }
}
