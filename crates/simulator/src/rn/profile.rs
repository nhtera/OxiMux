//! A CPU profile (`Profiler.stop`'s `profile`) as the functions that spent
//! the most time themselves. Each sample lasts until the next one (the last,
//! until the profile ends), and is charged to the function it caught; one
//! function's nodes (one per call path) are added up.

use std::collections::HashMap;

use serde_json::Value;

/// One function's share.
#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    pub name: String,
    pub url: String,
    /// 1-based.
    pub line: u32,
    pub self_ms: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub duration_ms: f64,
    /// Every function's self time, together.
    pub sampled_ms: f64,
    /// Most self time first, at most `top`.
    pub functions: Vec<Function>,
}

/// The `top` functions of `profile`; an error names what is missing.
pub fn summarize(profile: &Value, top: usize) -> Result<Summary, String> {
    let field = |name: &str| profile.get(name).ok_or_else(|| format!("the profile has no `{name}`"));
    let nodes = field("nodes")?.as_array().ok_or("the profile's nodes are not a list")?;
    let start = field("startTime")?.as_f64().ok_or("the profile has no start")?;
    let end = field("endTime")?.as_f64().ok_or("the profile has no end")?;
    let samples: Vec<u64> = list(profile.get("samples")).filter_map(Value::as_u64).collect();
    let deltas: Vec<f64> = list(profile.get("timeDeltas")).map(|d| d.as_f64().unwrap_or(0.0)).collect();

    // Node id → its function's key.
    let mut key_of: HashMap<u64, (String, String, u32)> = HashMap::new();
    for node in nodes {
        let (Some(id), Some(frame)) = (node.get("id").and_then(Value::as_u64), node.get("callFrame")) else { continue };
        let text = |k: &str| frame.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        // Hermes charges a sample with no JavaScript on the stack to its
        // root: the thread was idle (Chrome's "(idle)").
        let line = frame.get("lineNumber").and_then(Value::as_i64).map_or(0, |l| u32::try_from(l + 1).unwrap_or(0));
        let key = match text("functionName").as_str() {
            "[root]" | "(root)" => ("(idle)".to_owned(), String::new(), 0),
            "" => ("(anonymous)".to_owned(), text("url"), line),
            name => (name.to_owned(), text("url"), line),
        };
        key_of.insert(id, key);
    }

    // Sample i's time: from its timestamp to the next one's, in µs.
    let mut stamps = Vec::with_capacity(samples.len());
    let mut t = start;
    for i in 0..samples.len() {
        t += deltas.get(i).copied().unwrap_or(0.0);
        stamps.push(t);
    }
    let mut self_us: HashMap<&(String, String, u32), f64> = HashMap::new();
    for (i, id) in samples.iter().enumerate() {
        let next = stamps.get(i + 1).copied().unwrap_or(end);
        let Some(key) = key_of.get(id) else { continue };
        *self_us.entry(key).or_default() += (next - stamps[i]).max(0.0);
    }

    let mut functions: Vec<Function> = self_us
        .into_iter()
        .map(|((name, url, line), us)| Function { name: name.clone(), url: url.clone(), line: *line, self_ms: us / 1000.0 })
        .collect();
    let sampled_ms = functions.iter().map(|f| f.self_ms).sum();
    functions.sort_by(|a, b| b.self_ms.total_cmp(&a.self_ms).then_with(|| a.name.cmp(&b.name)));
    functions.truncate(top);
    Ok(Summary { duration_ms: ((end - start) / 1000.0).max(0.0), sampled_ms, functions })
}

fn list(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value.and_then(Value::as_array).into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = include_str!("../../tests/fixtures/rn/cpuprofile.json");

    #[test]
    fn self_time_adds_up_to_the_profile() {
        let profile: Value = serde_json::from_str(PROFILE).unwrap();
        let summary = summarize(&profile, 100).unwrap();
        assert!(summary.duration_ms > 0.0);
        let gap = (summary.duration_ms - summary.sampled_ms).abs() / summary.duration_ms;
        assert!(gap < 0.05, "sampled {} of {} ms", summary.sampled_ms, summary.duration_ms);
        let total: f64 = summary.functions.iter().map(|f| f.self_ms).sum();
        assert!((total - summary.sampled_ms).abs() < 1e-6, "every function is listed at --top 100");
        assert!(summary.functions.windows(2).all(|w| w[0].self_ms >= w[1].self_ms), "most first");
        // Hermes, live: an idle app, and the probe's hot spot.
        assert_eq!((summary.functions[0].name.as_str(), summary.functions[0].url.as_str()), ("(idle)", ""));
        assert!(summary.functions.iter().any(|f| f.name == "busyFibonacci" && f.url.contains("index.bundle")));
    }

    /// Two nodes of one function (two call paths) are one row; a sample
    /// lasts until the next.
    #[test]
    fn a_function_is_summed_across_its_call_paths() {
        let profile = serde_json::json!({
            "startTime": 0, "endTime": 10_000,
            "nodes": [
                { "id": 1, "callFrame": { "functionName": "(root)", "url": "", "lineNumber": -1 } },
                { "id": 2, "callFrame": { "functionName": "fib", "url": "http://localhost:8081/index.bundle", "lineNumber": 9 } },
                { "id": 3, "callFrame": { "functionName": "fib", "url": "http://localhost:8081/index.bundle", "lineNumber": 9 } },
                { "id": 4, "callFrame": { "functionName": "", "url": "http://localhost:8081/index.bundle", "lineNumber": 20 } }
            ],
            "samples": [2, 3, 4, 2],
            "timeDeltas": [0, 2000, 3000, 1000]
        });
        let summary = summarize(&profile, 2).unwrap();
        assert_eq!(summary.functions[0], Function { name: "fib".into(), url: "http://localhost:8081/index.bundle".into(), line: 10, self_ms: 9.0 });
        assert_eq!(summary.functions[1].name, "(anonymous)");
        assert_eq!(summary.functions.len(), 2);
        assert_eq!((summary.duration_ms, summary.sampled_ms), (10.0, 10.0));
        assert!(summarize(&serde_json::json!({}), 5).is_err());
    }
}
