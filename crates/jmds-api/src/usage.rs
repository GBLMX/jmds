//! What a turn cost, as DeepSeek reports it.
//!
//! The API has three shapes for the same accounting and this module normalises them into one:
//!
//! 1. `prompt_tokens` + `prompt_cache_hit_tokens` + `prompt_cache_miss_tokens`;
//! 2. `prompt_tokens` + `prompt_tokens_details.cached_tokens` (the miss has to be derived);
//! 3. `total_tokens` only (the output has to be derived).
//!
//! Two invariants matter more than the shapes:
//!
//! - **`reasoning_tokens` is never added to `output_tokens`.** It is reported inside
//!   `completion_tokens` already; adding it again would bill the same tokens twice.
//! - **A number that cannot be read is read as "missed the cache"**, never as "hit it". An
//!   accounting that guesses low is one that lies about what the user is spending.

/// One turn's tokens, normalised.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Everything sent, cached or not. `hit + miss` when the API splits it, `prompt_tokens`
    /// otherwise.
    pub input_tokens: u64,
    /// Input tokens served from the prefix cache — the cheap ones.
    pub prompt_cache_hit_tokens: u64,
    /// Input tokens the model had to process.
    pub prompt_cache_miss_tokens: u64,
    /// Everything generated, reasoning included.
    pub output_tokens: u64,
    /// The part of [`Self::output_tokens`] that was reasoning. Read-only: it is already inside
    /// `output_tokens` and must not be added to it.
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Read what a response (or a stream's final chunk) says.
    ///
    /// Missing fields are zero, and a shape that cannot be read falls back to the next one rather
    /// than to a guess that flatters the cache.
    pub fn from_json(value: &serde_json::Value) -> Self {
        let number = |path: &[&str]| -> Option<u64> {
            let mut node = value;
            for key in path {
                node = node.get(key)?;
            }
            node.as_u64()
                .or_else(|| node.as_i64().and_then(|n| u64::try_from(n).ok()))
        };

        let prompt = number(&["prompt_tokens"]);
        let hit = number(&["prompt_cache_hit_tokens"]);
        let miss = number(&["prompt_cache_miss_tokens"]);
        let cached = number(&["prompt_tokens_details", "cached_tokens"]);
        let completion = number(&["completion_tokens"]);
        let total = number(&["total_tokens"]);
        let reasoning = number(&["completion_tokens_details", "reasoning_tokens"]).unwrap_or(0);

        let input_tokens = prompt.or_else(|| match (hit, miss) {
            (Some(hit), Some(miss)) => Some(hit.saturating_add(miss)),
            _ => None,
        });

        // The split, from whichever pair is present: an explicit miss, else `prompt - cached`,
        // else `prompt - hit`, else "all of it missed".
        let (prompt_cache_hit_tokens, prompt_cache_miss_tokens) = match (input_tokens, hit, miss) {
            (Some(input), hit, Some(miss)) => {
                (hit.unwrap_or_else(|| input.saturating_sub(miss)), miss)
            }
            (Some(input), Some(hit), None) => (hit, input.saturating_sub(hit)),
            (Some(input), None, None) => match cached {
                Some(cached) => (cached.min(input), input.saturating_sub(cached.min(input))),
                None => (0, input),
            },
            // Nothing to split: the numbers stand as written.
            (None, hit, miss) => (hit.unwrap_or(0), miss.unwrap_or(0)),
        };

        let output_tokens = completion
            .or_else(|| total.map(|total| total.saturating_sub(input_tokens.unwrap_or(0))))
            .unwrap_or(0);

        Self {
            input_tokens: input_tokens.unwrap_or(0),
            prompt_cache_hit_tokens,
            prompt_cache_miss_tokens,
            output_tokens,
            reasoning_tokens: reasoning,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_split_is_taken_as_the_api_gives_it() {
        let usage = Usage::from_json(&json!({
            "prompt_tokens": 1000,
            "prompt_cache_hit_tokens": 900,
            "prompt_cache_miss_tokens": 100,
            "completion_tokens": 250,
            "total_tokens": 1250,
        }));
        assert_eq!(
            usage,
            Usage {
                input_tokens: 1000,
                prompt_cache_hit_tokens: 900,
                prompt_cache_miss_tokens: 100,
                output_tokens: 250,
                reasoning_tokens: 0,
            }
        );
    }

    #[test]
    fn a_miss_is_derived_when_only_the_hit_is_reported() {
        let usage = Usage::from_json(&json!({
            "prompt_tokens": 1000,
            "prompt_cache_hit_tokens": 850,
            "completion_tokens": 10,
        }));
        assert_eq!(usage.prompt_cache_hit_tokens, 850);
        assert_eq!(usage.prompt_cache_miss_tokens, 150);
        assert_eq!(usage.input_tokens, 1000);
    }

    #[test]
    fn a_miss_is_derived_from_cached_tokens_when_that_is_all_there_is() {
        let usage = Usage::from_json(&json!({
            "prompt_tokens": 1000,
            "prompt_tokens_details": { "cached_tokens": 640 },
            "completion_tokens": 10,
        }));
        assert_eq!(usage.prompt_cache_hit_tokens, 640);
        assert_eq!(usage.prompt_cache_miss_tokens, 360);
    }

    #[test]
    fn a_number_that_is_missing_is_read_as_a_miss_not_as_a_hit() {
        // The failure this guards against is an accounting that flatters the cache: a report with
        // no cache fields at all is "nothing was cached", which is also the expensive reading.
        let usage = Usage::from_json(&json!({ "prompt_tokens": 500, "completion_tokens": 5 }));
        assert_eq!(usage.prompt_cache_hit_tokens, 0);
        assert_eq!(usage.prompt_cache_miss_tokens, 500);
    }

    #[test]
    fn reasoning_tokens_are_reported_but_never_added_to_the_output() {
        let usage = Usage::from_json(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 800,
            "completion_tokens_details": { "reasoning_tokens": 600 },
        }));
        assert_eq!(
            usage.output_tokens, 800,
            "reasoning is already inside completion"
        );
        assert_eq!(usage.reasoning_tokens, 600);
    }

    #[test]
    fn an_output_is_derived_from_the_total_when_completion_is_absent() {
        let usage = Usage::from_json(&json!({ "prompt_tokens": 300, "total_tokens": 380 }));
        assert_eq!(usage.output_tokens, 80);
    }

    #[test]
    fn a_total_smaller_than_the_prompt_cannot_wrap_around() {
        let usage = Usage::from_json(&json!({ "prompt_tokens": 500, "total_tokens": 100 }));
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn the_hit_is_never_larger_than_the_input_it_was_taken_from() {
        // A provider bug, or a field borrowed from another shape; either way the split must stay
        // addable back up to the input.
        let usage = Usage::from_json(&json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": { "cached_tokens": 999 },
            "completion_tokens": 1,
        }));
        assert_eq!(usage.prompt_cache_hit_tokens, 100);
        assert_eq!(usage.prompt_cache_miss_tokens, 0);
        assert_eq!(
            usage.prompt_cache_hit_tokens + usage.prompt_cache_miss_tokens,
            usage.input_tokens
        );
    }

    #[test]
    fn an_empty_report_is_empty_and_not_a_panic() {
        let usage = Usage::from_json(&json!({}));
        assert!(usage.is_empty());
        let usage = Usage::from_json(&json!({ "prompt_tokens": -5 }));
        assert_eq!(usage.input_tokens, 0, "a negative count is not a count");
    }
}
