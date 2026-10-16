//! Prometheus metrics.
//!
//! Gauges may be *deferred*: a callback sampled at scrape time, so process-wide
//! state (RSS, session count) is reported without the runtime pushing updates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type Labels = Vec<(String, String)>;

fn label_key(labels: &Labels) -> String {
    let mut l = labels.clone();
    l.sort();
    l.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}

fn fmt_labels(labels: &Labels) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut l = labels.clone();
    l.sort();
    let inner = l
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{inner}}}")
}

#[derive(Default)]
struct Histogram {
    bounds: Vec<f64>,
    buckets: Vec<AtomicU64>,
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new(bounds: Vec<f64>) -> Self {
        let n = bounds.len();
        Self {
            bounds,
            buckets: (0..n + 1).map(|_| AtomicU64::new(0)).collect(),
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
    fn observe(&self, v: f64) {
        let mut idx = self.bounds.len();
        for (i, b) in self.bounds.iter().enumerate() {
            if v <= *b {
                idx = i;
                break;
            }
        }
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add((v * 1_000_000.0) as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

struct CounterFamily {
    help: &'static str,
    values: Mutex<HashMap<String, Arc<AtomicU64>>>,
}

struct GaugeFamily {
    help: &'static str,
    values: Mutex<HashMap<String, Arc<dyn Fn() -> f64 + Send + Sync>>>,
}

struct HistFamily {
    help: &'static str,
    values: Mutex<HashMap<String, Arc<Histogram>>>,
}

pub struct Metrics {
    counters: Mutex<HashMap<&'static str, Arc<CounterFamily>>>,
    gauges: Mutex<HashMap<&'static str, Arc<GaugeFamily>>>,
    histograms: Mutex<HashMap<&'static str, Arc<HistFamily>>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            counters: Mutex::new(HashMap::new()),
            gauges: Mutex::new(HashMap::new()),
            histograms: Mutex::new(HashMap::new()),
        }
    }

    fn counter_family(&self, name: &'static str, help: &'static str) -> Arc<CounterFamily> {
        let mut map = self.counters.lock().unwrap();
        map.entry(name)
            .or_insert_with(|| {
                Arc::new(CounterFamily {
                    help,
                    values: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    pub fn inc(&self, name: &'static str, help: &'static str, labels: &[(&str, &str)]) {
        self.add(name, help, labels, 1);
    }

    pub fn add(&self, name: &'static str, help: &'static str, labels: &[(&str, &str)], delta: u64) {
        let fam = self.counter_family(name, help);
        let key = label_key(&labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>());
        let cell = {
            let mut vals = fam.values.lock().unwrap();
            vals.entry(key).or_insert_with(|| Arc::new(AtomicU64::new(0))).clone()
        };
        cell.fetch_add(delta, Ordering::Relaxed);
    }

    /// Register a gauge sampled at scrape time.
    pub fn gauge(
        &self,
        name: &'static str,
        help: &'static str,
        labels: &[(&str, &str)],
        f: impl Fn() -> f64 + Send + Sync + 'static,
    ) {
        let fam = {
            let mut map = self.gauges.lock().unwrap();
            map.entry(name)
                .or_insert_with(|| {
                    Arc::new(GaugeFamily {
                        help,
                        values: Mutex::new(HashMap::new()),
                    })
                })
                .clone()
        };
        let key = label_key(&labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>());
        fam.values.lock().unwrap().insert(key, Arc::new(f));
    }

    pub fn observe(&self, name: &'static str, help: &'static str, labels: &[(&str, &str)], v: f64) {
        let fam = {
            let mut map = self.histograms.lock().unwrap();
            map.entry(name)
                .or_insert_with(|| {
                    Arc::new(HistFamily {
                        help,
                        values: Mutex::new(HashMap::new()),
                    })
                })
                .clone()
        };
        let key = label_key(&labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>());
        let h = {
            let mut vals = fam.values.lock().unwrap();
            vals.entry(key)
                .or_insert_with(|| {
                    Arc::new(Histogram::new(vec![
                        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
                    ]))
                })
                .clone()
        };
        h.observe(v);
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(4096);

        let counters: Vec<(String, Arc<CounterFamily>)> = {
            let map = self.counters.lock().unwrap();
            map.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        };
        for (name, fam) in counters {
            out.push_str(&format!("# HELP {name} {}\n# TYPE {name} counter\n", fam.help));
            let vals = fam.values.lock().unwrap();
            for (key, cell) in vals.iter() {
                let labels = parse_label_key(key);
                out.push_str(&format!("{name}{} {}\n", fmt_labels(&labels), cell.load(Ordering::Relaxed)));
            }
        }

        let gauges: Vec<(String, Arc<GaugeFamily>)> = {
            let map = self.gauges.lock().unwrap();
            map.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        };
        for (name, fam) in gauges {
            out.push_str(&format!("# HELP {name} {}\n# TYPE {name} gauge\n", fam.help));
            let vals = fam.values.lock().unwrap();
            for (key, f) in vals.iter() {
                let labels = parse_label_key(key);
                out.push_str(&format!("{name}{} {}\n", fmt_labels(&labels), f()));
            }
        }

        let hists: Vec<(String, Arc<HistFamily>)> = {
            let map = self.histograms.lock().unwrap();
            map.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        };
        for (name, fam) in hists {
            out.push_str(&format!("# HELP {name} {}\n# TYPE {name} histogram\n", fam.help));
            let vals = fam.values.lock().unwrap();
            for (key, h) in vals.iter() {
                let base = parse_label_key(key);
                let mut cumulative = 0u64;
                for (i, bound) in h.bounds.iter().enumerate() {
                    cumulative += h.buckets[i].load(Ordering::Relaxed);
                    let mut l = base.clone();
                    l.push(("le".into(), bound.to_string()));
                    out.push_str(&format!("{name}_bucket{} {}\n", fmt_labels(&l), cumulative));
                }
                cumulative += h.buckets[h.bounds.len()].load(Ordering::Relaxed);
                let mut l = base.clone();
                l.push(("le".into(), "+Inf".into()));
                out.push_str(&format!("{name}_bucket{} {}\n", fmt_labels(&l), cumulative));
                out.push_str(&format!(
                    "{name}_sum{} {}\n",
                    fmt_labels(&base),
                    h.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
                ));
                out.push_str(&format!(
                    "{name}_count{} {}\n",
                    fmt_labels(&base),
                    h.count.load(Ordering::Relaxed)
                ));
            }
        }
        out
    }
}

fn parse_label_key(key: &str) -> Labels {
    if key.is_empty() {
        return Vec::new();
    }
    key.split(',')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            match (it.next(), it.next()) {
                (Some(k), Some(v)) => Some((k.to_string(), v.to_string())),
                _ => None,
            }
        })
        .collect()
}

/// Resident set size in bytes, read from /proc (Linux only; 0 elsewhere).
pub fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
            if let Some(pages) = s.split_whitespace().nth(1) {
                if let Ok(p) = pages.parse::<u64>() {
                    return p * 4096;
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}
