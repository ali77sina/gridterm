use serde_json::Value;

use super::{TokenKind, UsageStore};

/// Start an in-process OTLP/HTTP (JSON) collector on 127.0.0.1. Coding agents
/// (Claude Code, Codex, anything OTEL-capable) are pointed here via env vars,
/// and POST their token/cost metrics, which we attribute to the pane that
/// started them. Returns the bound port, or None if it couldn't bind.
pub fn start(store: UsageStore) -> Option<u16> {
    // Bind to an ephemeral port on loopback only.
    let server = tiny_http::Server::http("127.0.0.1:0").ok()?;
    let port = server.server_addr().to_ip().map(|a| a.port())?;

    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            // Only metrics matter; accept any /v1/* path.
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            if let Ok(json) = serde_json::from_str::<Value>(&body) {
                parse_metrics(&json, &store);
            }
            let _ = request.respond(tiny_http::Response::from_string("{}"));
        }
    });

    Some(port)
}

/// Parse an OTLP/HTTP JSON ExportMetricsServiceRequest and fold token/cost
/// data points into the usage store, attributed by the gridterm.pane resource
/// attribute we inject into each shell.
fn parse_metrics(json: &Value, store: &UsageStore) {
    let Some(resource_metrics) = json.get("resourceMetrics").and_then(|v| v.as_array()) else {
        return;
    };
    for rm in resource_metrics {
        // Pane index from resource attributes (gridterm.pane), default 0.
        let pane = attr_int(
            rm.get("resource")
                .and_then(|r| r.get("attributes")),
            "gridterm.pane",
        )
        .unwrap_or(0) as usize;

        let Some(scope_metrics) = rm.get("scopeMetrics").and_then(|v| v.as_array()) else {
            continue;
        };
        for sm in scope_metrics {
            let Some(metrics) = sm.get("metrics").and_then(|v| v.as_array()) else {
                continue;
            };
            for metric in metrics {
                let name = metric.get("name").and_then(|v| v.as_str()).unwrap_or("");
                // sum or gauge -> dataPoints
                let dps = metric
                    .get("sum")
                    .or_else(|| metric.get("gauge"))
                    .and_then(|d| d.get("dataPoints"))
                    .and_then(|v| v.as_array());
                let Some(dps) = dps else { continue };

                for dp in dps {
                    let attrs = dp.get("attributes");
                    let model = attr_str(attrs, "model").unwrap_or_default();
                    let value = dp_value(dp);

                    match name {
                        "claude_code.token.usage" | "claude_code.token.usage.tokens" => {
                            let kind = match attr_str(attrs, "type").as_deref() {
                                Some("input") => TokenKind::Input,
                                Some("output") => TokenKind::Output,
                                Some("cacheRead") => TokenKind::CacheRead,
                                Some("cacheCreation") => TokenKind::CacheCreation,
                                _ => TokenKind::Input,
                            };
                            store.add_tokens(pane, &model, kind, value as u64);
                        }
                        "claude_code.cost.usage" | "claude_code.cost.usage.USD" => {
                            store.add_cost(pane, &model, value);
                        }
                        // Generic OTEL GenAI token metrics (Codex / others).
                        n if n.contains("token") => {
                            let kind = match attr_str(attrs, "gen_ai.token.type")
                                .or_else(|| attr_str(attrs, "type"))
                                .as_deref()
                            {
                                Some("output") | Some("completion") => TokenKind::Output,
                                Some("cacheRead") => TokenKind::CacheRead,
                                _ => TokenKind::Input,
                            };
                            store.add_tokens(pane, &model, kind, value as u64);
                        }
                        n if n.contains("cost") => {
                            store.add_cost(pane, &model, value);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Read a numeric data-point value (asInt or asDouble).
fn dp_value(dp: &Value) -> f64 {
    if let Some(i) = dp.get("asInt") {
        // asInt may be a string or number in OTLP JSON.
        return i
            .as_i64()
            .map(|v| v as f64)
            .or_else(|| i.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0.0);
    }
    if let Some(d) = dp.get("asDouble") {
        return d.as_f64().unwrap_or(0.0);
    }
    0.0
}

/// Find a string attribute by key in an OTLP attributes array.
fn attr_str(attrs: Option<&Value>, key: &str) -> Option<String> {
    let arr = attrs?.as_array()?;
    for a in arr {
        if a.get("key").and_then(|k| k.as_str()) == Some(key) {
            return a
                .get("value")
                .and_then(|v| v.get("stringValue"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

/// Find an integer attribute by key (stringValue or intValue).
fn attr_int(attrs: Option<&Value>, key: &str) -> Option<i64> {
    let arr = attrs?.as_array()?;
    for a in arr {
        if a.get("key").and_then(|k| k.as_str()) == Some(key) {
            let v = a.get("value")?;
            if let Some(i) = v.get("intValue") {
                return i.as_i64().or_else(|| i.as_str().and_then(|s| s.parse().ok()));
            }
            if let Some(s) = v.get("stringValue").and_then(|s| s.as_str()) {
                return s.parse().ok();
            }
        }
    }
    None
}
