use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub mod collector;

/// How a given agent/session is billed.
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)] // Subscription mode is wired for quota users; UI toggle TBD.
pub enum BillingMode {
    /// Pay-per-token (API key, incl. Bedrock): we can show real dollars.
    Api,
    /// Subscription/quota (Claude Pro, ChatGPT): show quota %/tokens, not $.
    Subscription,
}

/// Accumulated usage for one agent session.
#[derive(Default, Clone)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Cost in USD as reported by the agent (when it emits cost.usage).
    pub reported_cost_usd: f64,
    /// Cost in USD computed by us from tokens x price sheet (for API mode).
    pub computed_cost_usd: f64,
    /// Most recent model seen for this session.
    pub model: String,
}

impl SessionUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens
    }

    /// Best available cost estimate: prefer the agent's own reported cost,
    /// else our computed cost from the price sheet.
    pub fn best_cost(&self) -> f64 {
        if self.reported_cost_usd > 0.0 {
            self.reported_cost_usd
        } else {
            self.computed_cost_usd
        }
    }
}

/// Per-model pricing in USD per 1M tokens (input, output, cache-read).
#[derive(Clone, Copy)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

/// Look up a price sheet entry by (fuzzy) model name. Returns None for unknown
/// models (we then fall back to the agent's reported cost).
fn price_for(model: &str) -> Option<Price> {
    let m = model.to_lowercase();
    // A small, current sheet of common coding-agent models ($/1M tokens).
    // Bedrock Claude pricing matches Anthropic list pricing for these tiers.
    let p = |input, output, cache_read| Some(Price { input, output, cache_read });
    if m.contains("opus") {
        p(15.0, 75.0, 1.50)
    } else if m.contains("sonnet") {
        p(3.0, 15.0, 0.30)
    } else if m.contains("haiku") {
        p(0.80, 4.0, 0.08)
    } else if m.contains("gpt-5") || m.contains("codex") {
        p(1.25, 10.0, 0.13)
    } else if m.contains("o3") || m.contains("o4") {
        p(2.0, 8.0, 0.50)
    } else {
        None
    }
}

/// Compute USD cost for a session from its tokens + the model price sheet.
fn compute_cost(u: &SessionUsage) -> f64 {
    let Some(p) = price_for(&u.model) else {
        return 0.0;
    };
    let m = 1_000_000.0;
    (u.input_tokens as f64 / m) * p.input
        + (u.output_tokens as f64 / m) * p.output
        + (u.cache_read_tokens as f64 / m) * p.cache_read
        + (u.cache_creation_tokens as f64 / m) * p.input // cache creation ~ input price
}

/// Shared, thread-safe usage store. The OTLP collector thread writes; the UI
/// reads. Sessions are mapped to panes by the pane index injected via env.
#[derive(Clone, Default)]
pub struct UsageStore {
    inner: Arc<Mutex<UsageInner>>,
}

#[derive(Default)]
struct UsageInner {
    /// Per-pane aggregated usage (pane index -> usage). Sessions started in a
    /// pane are folded into that pane's totals.
    per_pane: HashMap<usize, SessionUsage>,
    /// Billing mode per pane (defaults to Api).
    mode: HashMap<usize, BillingMode>,
}

impl UsageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record token usage for a pane (called by the collector).
    pub fn add_tokens(
        &self,
        pane: usize,
        model: &str,
        kind: TokenKind,
        amount: u64,
    ) {
        let mut g = self.inner.lock().unwrap();
        let u = g.per_pane.entry(pane).or_default();
        if !model.is_empty() {
            u.model = model.to_string();
        }
        match kind {
            TokenKind::Input => u.input_tokens += amount,
            TokenKind::Output => u.output_tokens += amount,
            TokenKind::CacheRead => u.cache_read_tokens += amount,
            TokenKind::CacheCreation => u.cache_creation_tokens += amount,
        }
        u.computed_cost_usd = compute_cost(u);
    }

    /// Record an agent-reported cost (USD) for a pane.
    pub fn add_cost(&self, pane: usize, model: &str, usd: f64) {
        let mut g = self.inner.lock().unwrap();
        let u = g.per_pane.entry(pane).or_default();
        if !model.is_empty() {
            u.model = model.to_string();
        }
        u.reported_cost_usd += usd;
    }

    #[allow(dead_code)] // Used when a pane is marked as subscription-billed.
    pub fn set_mode(&self, pane: usize, mode: BillingMode) {
        self.inner.lock().unwrap().mode.insert(pane, mode);
    }

    /// Snapshot one pane's usage (for the on-screen badge).
    pub fn pane(&self, pane: usize) -> Option<SessionUsage> {
        self.inner.lock().unwrap().per_pane.get(&pane).cloned()
    }

    /// Total cost across all panes (USD).
    pub fn total_cost(&self) -> f64 {
        self.inner
            .lock()
            .unwrap()
            .per_pane
            .values()
            .map(|u| u.best_cost())
            .sum()
    }

    /// A compact text report for the AI usage tool.
    pub fn report(&self) -> String {
        let g = self.inner.lock().unwrap();
        if g.per_pane.is_empty() {
            return "No agent usage captured yet. (Telemetry is auto-enabled for \
Claude Code and Codex started inside gridterm panes.)"
                .into();
        }
        let mut out = String::new();
        let mut total = 0.0;
        let mut panes: Vec<_> = g.per_pane.iter().collect();
        panes.sort_by_key(|(k, _)| **k);
        for (pane, u) in panes {
            let cost = u.best_cost();
            total += cost;
            let mode = g.mode.get(pane).copied().unwrap_or(BillingMode::Api);
            match mode {
                BillingMode::Api => out.push_str(&format!(
                    "pane {pane} [{}]: ${cost:.4}  (in {}, out {}, cache {} tokens)\n",
                    if u.model.is_empty() { "?" } else { &u.model },
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_tokens
                )),
                BillingMode::Subscription => out.push_str(&format!(
                    "pane {pane} [{}]: subscription — {} tokens (in {}, out {})\n",
                    if u.model.is_empty() { "?" } else { &u.model },
                    u.total_tokens(),
                    u.input_tokens,
                    u.output_tokens
                )),
            }
        }
        out.push_str(&format!("\nTotal (API-billed panes): ${total:.4}"));
        out
    }
}

#[derive(Clone, Copy)]
pub enum TokenKind {
    Input,
    Output,
    CacheRead,
    CacheCreation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_captures_metrics() {
        let store = UsageStore::new();
        let port = collector::start(store.clone()).expect("bind");

        let body = serde_json::json!({
            "resourceMetrics": [{
                "resource": { "attributes": [
                    { "key": "gridterm.pane", "value": { "intValue": "2" } }
                ]},
                "scopeMetrics": [{ "metrics": [
                    { "name": "claude_code.token.usage", "sum": { "dataPoints": [
                        { "asInt": "1000", "attributes": [
                            { "key": "type", "value": { "stringValue": "input" } },
                            { "key": "model", "value": { "stringValue": "claude-sonnet-4" } }]},
                        { "asInt": "500", "attributes": [
                            { "key": "type", "value": { "stringValue": "output" } },
                            { "key": "model", "value": { "stringValue": "claude-sonnet-4" } }]}
                    ]}},
                    { "name": "claude_code.cost.usage", "sum": { "dataPoints": [
                        { "asDouble": 0.012, "attributes": [
                            { "key": "model", "value": { "stringValue": "claude-sonnet-4" } }]}
                    ]}}
                ]}]
            }]
        });

        let url = format!("http://127.0.0.1:{port}/v1/metrics");
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .expect("post");

        std::thread::sleep(std::time::Duration::from_millis(250));

        let report = store.report();
        assert!(report.contains("pane 2"), "missing pane: {report}");
        assert!(store.total_cost() > 0.0, "total should be > 0: {report}");
        let u = store.inner.lock().unwrap();
        let p = u.per_pane.get(&2).unwrap();
        assert_eq!(p.input_tokens, 1000);
        assert_eq!(p.output_tokens, 500);
    }
}
