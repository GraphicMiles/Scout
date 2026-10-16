/** Extract the balanced JSON object that follows `marker` in a script body (string/escape aware). */
export function extractJsonAfter(src, marker) {
  const at = src.indexOf(marker);
  if (at < 0) return null;
  const start = src.indexOf('{', at + marker.length);
  if (start < 0) return null;
  let depth = 0; let inStr = false; let esc = false;
  for (let j = start; j < src.length; j++) {
    const ch = src[j];
    if (inStr) { if (esc) esc = false; else if (ch === '\\') esc = true; else if (ch === '"') inStr = false; continue; }
    if (ch === '"') inStr = true;
    else if (ch === '{') depth++;
    else if (ch === '}') { depth--; if (depth === 0) return src.slice(start, j + 1); }
  }
  return null;
}

/** Iterative DFS over parsed JSON, bounded. */
export function* walkJson(root, max = 500000) {
  const st = [root];
  let seen = 0;
  while (st.length && seen++ < max) {
    const v = st.pop();
    if (v && typeof v === 'object') {
      yield v;
      if (Array.isArray(v)) { for (let i = v.length - 1; i >= 0; i--) st.push(v[i]); }
      else { const ks = Object.keys(v); for (let i = ks.length - 1; i >= 0; i--) st.push(v[ks[i]]); }
    }
  }
}

export function resolveUrl(href, base) {
  try { return new URL(href, base).href; } catch { return null; }
}
