const BUCKETS = [0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60];
const esc = (v) => String(v).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n');
const fmt = (labels) => {
  const e = Object.entries(labels).sort();
  return e.length ? '{' + e.map(([k, v]) => `${k}="${esc(v)}"`).join(',') + '}' : '';
};

export class Metrics {
  #counters = new Map();
  #hists = new Map();
  #gauges = new Map();

  inc(name, labels = {}, v = 1) {
    const k = name + fmt(labels);
    const c = this.#counters.get(k) ?? { name, labels, value: 0 };
    c.value += v;
    this.#counters.set(k, c);
  }
  value(name, labels = {}) { return this.#counters.get(name + fmt(labels))?.value ?? 0; }

  observe(name, labels, seconds) {
    const k = name + fmt(labels);
    let h = this.#hists.get(k);
    if (!h) { h = { name, labels, counts: new Array(BUCKETS.length).fill(0), sum: 0, count: 0 }; this.#hists.set(k, h); }
    h.sum += seconds; h.count += 1;
    BUCKETS.forEach((b, i) => { if (seconds <= b) h.counts[i] += 1; });
  }

  gauge(name, fn) { this.#gauges.set(name, fn); }

  render() {
    const out = [];
    for (const [name, fn] of this.#gauges) { out.push(`# TYPE ${name} gauge`, `${name} ${Number(fn())}`); }
    const seen = new Set();
    for (const c of this.#counters.values()) {
      if (!seen.has(c.name)) { out.push(`# TYPE ${c.name} counter`); seen.add(c.name); }
      out.push(`${c.name}${fmt(c.labels)} ${c.value}`);
    }
    const seenH = new Set();
    for (const h of this.#hists.values()) {
      if (!seenH.has(h.name)) { out.push(`# TYPE ${h.name} histogram`); seenH.add(h.name); }
      BUCKETS.forEach((b, i) => out.push(`${h.name}_bucket${fmt({ ...h.labels, le: b })} ${h.counts[i]}`));
      out.push(`${h.name}_bucket${fmt({ ...h.labels, le: '+Inf' })} ${h.count}`);
      out.push(`${h.name}_sum${fmt(h.labels)} ${h.sum}`, `${h.name}_count${fmt(h.labels)} ${h.count}`);
    }
    return out.join('\n') + '\n';
  }
}
