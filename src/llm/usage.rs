//! Native cache accounting survives normalization without treating TTLs as interchangeable.

use serde_json::Value;

use super::Usage;

/// Sum of reported tokens, with coverage so partial reports never masquerade as complete totals.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CacheCounter {
    /// Absent is different from a reported zero.
    pub tokens: Option<u64>,
    /// Number of model steps reporting this field, not number of streaming events.
    pub reports: u64,
}

impl CacheCounter {
    fn from_value(value: &Value) -> Self {
        let tokens = value.as_u64();
        Self {
            tokens,
            reports: u64::from(tokens.is_some()),
        }
    }

    fn add(&mut self, other: &Self) {
        if let Some(tokens) = other.tokens {
            self.tokens = Some(self.tokens.unwrap_or(0).saturating_add(tokens));
        }
        self.reports = self.reports.saturating_add(other.reports);
    }
}

/// Native accounting families, not inferred cache hits or a universal billing model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheUsage {
    /// OpenAI-shaped details, also returned by documented compatible endpoints.
    OpenAi {
        /// Billed/read cached input, already included in total input.
        cached: CacheCounter,
        /// Cache-write total, not added again to its breakdown.
        written: CacheCounter,
        /// Reported short-lifetime writes.
        ttl_30m: CacheCounter,
        /// Reported extended-lifetime writes.
        ttl_12h: CacheCounter,
    },
    /// Anthropic's separate uncached/read/creation input semantics.
    Anthropic {
        /// Cache-read input.
        read: CacheCounter,
        /// Cache-creation input total.
        created: CacheCounter,
        /// Short-lifetime creation breakdown.
        ephemeral_5m: CacheCounter,
        /// Extended-lifetime creation breakdown.
        ephemeral_1h: CacheCounter,
    },
    /// DeepSeek reports hits and misses directly rather than OpenAI details.
    DeepSeek {
        /// Provider-reported cache hits.
        hit: CacheCounter,
        /// Provider-reported cache misses.
        miss: CacheCounter,
    },
}

impl CacheUsage {
    fn counters(&mut self) -> Vec<&mut CacheCounter> {
        match self {
            Self::OpenAi {
                cached,
                written,
                ttl_30m,
                ttl_12h,
            } => vec![cached, written, ttl_30m, ttl_12h],
            Self::Anthropic {
                read,
                created,
                ephemeral_5m,
                ephemeral_1h,
            } => vec![read, created, ephemeral_5m, ephemeral_1h],
            Self::DeepSeek { hit, miss } => vec![hit, miss],
        }
    }

    fn is_reported(&self) -> bool {
        let mut copy = self.clone();
        copy.counters()
            .iter()
            .any(|counter| counter.tokens.is_some())
    }
}

impl Usage {
    /// Overlay cumulative streaming reports without counting an event as a model step.
    pub(super) fn update_snapshot(&mut self, update: Self) {
        self.input = update.input.or(self.input);
        self.output = update.output.or(self.output);
        for incoming in update.cache {
            if let Some(existing) = self
                .cache
                .iter_mut()
                .find(|value| std::mem::discriminant(*value) == std::mem::discriminant(&incoming))
            {
                for (existing, count) in existing
                    .counters()
                    .into_iter()
                    .zip(incoming.clone().counters())
                {
                    if count.tokens.is_some() {
                        *existing = count.clone();
                    }
                }
            } else {
                self.cache.push(incoming);
            }
        }
    }

    pub(super) fn openai(value: &Value, responses: bool) -> Self {
        let details = &value[if responses {
            "input_tokens_details"
        } else {
            "prompt_tokens_details"
        }];
        let mut cache = Vec::new();
        for usage in [
            CacheUsage::OpenAi {
                cached: CacheCounter::from_value(&details["cached_tokens"]),
                written: CacheCounter::from_value(&details["cache_write_tokens"]),
                ttl_30m: CacheCounter::from_value(
                    &details["cache_write_tokens_details"]["ttl_30m_tokens"],
                ),
                ttl_12h: CacheCounter::from_value(
                    &details["cache_write_tokens_details"]["ttl_12h_tokens"],
                ),
            },
            CacheUsage::DeepSeek {
                hit: CacheCounter::from_value(&value["prompt_cache_hit_tokens"]),
                miss: CacheCounter::from_value(&value["prompt_cache_miss_tokens"]),
            },
        ] {
            if usage.is_reported() {
                cache.push(usage);
            }
        }
        Self {
            input: value[if responses {
                "input_tokens"
            } else {
                "prompt_tokens"
            }]
            .as_u64(),
            output: value[if responses {
                "output_tokens"
            } else {
                "completion_tokens"
            }]
            .as_u64(),
            cache,
        }
    }

    pub(super) fn anthropic(value: &Value) -> Self {
        let details = CacheUsage::Anthropic {
            read: CacheCounter::from_value(&value["cache_read_input_tokens"]),
            created: CacheCounter::from_value(&value["cache_creation_input_tokens"]),
            ephemeral_5m: CacheCounter::from_value(
                &value["cache_creation"]["ephemeral_5m_input_tokens"],
            ),
            ephemeral_1h: CacheCounter::from_value(
                &value["cache_creation"]["ephemeral_1h_input_tokens"],
            ),
        };
        Self {
            // Anthropic's input_tokens excludes both creation and reads; OpenAI's does not.
            input: value["input_tokens"].as_u64().map(|input| {
                input
                    .saturating_add(value["cache_creation_input_tokens"].as_u64().unwrap_or(0))
                    .saturating_add(value["cache_read_input_tokens"].as_u64().unwrap_or(0))
            }),
            output: value["output_tokens"].as_u64(),
            cache: if details.is_reported() {
                vec![details]
            } else {
                vec![]
            },
        }
    }

    /// Accumulate completed steps only; streaming usage is cumulative within one step.
    pub(crate) fn accumulate(&mut self, step: &Self) {
        for (total, count) in [
            (&mut self.input, step.input),
            (&mut self.output, step.output),
        ] {
            if let Some(count) = count {
                *total = Some(total.unwrap_or(0).saturating_add(count));
            }
        }
        for incoming in &step.cache {
            if let Some(total) = self
                .cache
                .iter_mut()
                .find(|total| std::mem::discriminant(*total) == std::mem::discriminant(incoming))
            {
                for (total, count) in total
                    .counters()
                    .into_iter()
                    .zip(incoming.clone().counters())
                {
                    total.add(count);
                }
            } else {
                self.cache.push(incoming.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_cache_breakdowns_are_preserved_without_adding_them_to_input_twice() {
        let openai = Usage::openai(
            &json!({"input_tokens":12000,"output_tokens":5,"input_tokens_details":{
                "cached_tokens":4000,"cache_write_tokens":7000,"cache_write_tokens_details":{"ttl_30m_tokens":3000,"ttl_12h_tokens":4000}
            }}),
            true,
        );
        assert_eq!(openai.input, Some(12000));
        let CacheUsage::OpenAi {
            cached,
            written,
            ttl_30m,
            ttl_12h,
        } = &openai.cache[0]
        else {
            panic!("OpenAI details expected")
        };
        assert_eq!(cached.tokens, Some(4000));
        assert_eq!(written.tokens, Some(7000));
        assert_eq!(ttl_30m.tokens, Some(3000));
        assert_eq!(ttl_12h.tokens, Some(4000));
        let anthropic = Usage::anthropic(
            &json!({"input_tokens":50,"cache_read_input_tokens":1000,"cache_creation_input_tokens":2000,
            "cache_creation":{"ephemeral_5m_input_tokens":1500,"ephemeral_1h_input_tokens":500}}),
        );
        assert_eq!(anthropic.input, Some(3050));
        let CacheUsage::Anthropic {
            read,
            created,
            ephemeral_5m,
            ephemeral_1h,
        } = &anthropic.cache[0]
        else {
            panic!("Anthropic details expected")
        };
        assert_eq!(read.tokens, Some(1000));
        assert_eq!(created.tokens, Some(2000));
        assert_eq!(ephemeral_5m.tokens, Some(1500));
        assert_eq!(ephemeral_1h.tokens, Some(500));
    }

    #[test]
    fn partial_cache_totals_retain_missing_zero_coverage_and_saturating_counts() {
        let mut total = Usage::default();
        for value in [
            json!({}),
            json!({"prompt_tokens_details":{"cached_tokens":0}}),
            json!({"prompt_tokens_details":{"cached_tokens":u64::MAX,"cache_write_tokens":10}}),
            json!({"prompt_tokens_details":{"cached_tokens":10}}),
        ] {
            total.accumulate(&Usage::openai(&value, false));
        }
        let CacheUsage::OpenAi {
            cached,
            written,
            ttl_30m,
            ..
        } = &total.cache[0]
        else {
            panic!("OpenAI details expected")
        };
        assert_eq!(cached.tokens, Some(u64::MAX));
        assert_eq!(cached.reports, 3);
        assert_eq!(written.tokens, Some(10));
        assert_eq!(written.reports, 1);
        assert_eq!(ttl_30m.tokens, None);
        assert_eq!(ttl_30m.reports, 0);
        assert!(Usage::openai(&json!({}), false).cache.is_empty());
    }

    #[test]
    fn cumulative_streaming_snapshots_replace_reported_fields_and_preserve_omitted_ones() {
        let mut usage = Usage::openai(
            &json!({"prompt_tokens":1000,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":500}}),
            false,
        );
        usage.update_snapshot(Usage::openai(
            &json!({"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":300}}),
            false,
        ));
        let CacheUsage::OpenAi {
            cached, written, ..
        } = &usage.cache[0]
        else {
            panic!("OpenAI details expected")
        };
        assert_eq!(usage.input, Some(1000));
        assert_eq!(usage.output, Some(20));
        assert_eq!(cached.tokens, Some(300));
        assert_eq!(cached.reports, 1);
        assert_eq!(written.tokens, Some(500));
        assert_eq!(written.reports, 1);
    }

    #[test]
    fn deepseek_hit_and_miss_reports_retain_their_native_identity() {
        let usage = Usage::openai(
            &json!({"prompt_tokens":120,"prompt_cache_hit_tokens":100,"prompt_cache_miss_tokens":20}),
            false,
        );
        let CacheUsage::DeepSeek { hit, miss } = &usage.cache[0] else {
            panic!("DeepSeek details expected")
        };
        assert_eq!(hit.tokens, Some(100));
        assert_eq!(miss.tokens, Some(20));
        assert_eq!(usage.input, Some(120));
    }
}
