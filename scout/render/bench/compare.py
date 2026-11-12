#!/usr/bin/env python3
"""Compare our render with Chromium's, site by site.

Three numbers per site, because "how close is the page" and "where is this box"
are different questions and a single figure hides which one failed:

  abs   — of the elements both engines placed, how many have every edge within
          2 px of Chromium's. This is whole-page fidelity: one wrong line height
          early in the document shifts everything below it and fails them all.
  local — the same test, but with each box measured *relative to its parent*
          (offset from the parent's border box, plus its own size). This asks
          "is this box in the right place inside its container", and is immune
          to accumulated vertical drift and to font substitution.
  ssim  — structural similarity of the two screenshots, cropped to the smaller
          of the two so a page taller than the viewport is not penalised for the
          part neither engine drew.

Writes bench/results.md.
"""

import json
import pathlib
import sys

import numpy as np
from PIL import Image

HERE = pathlib.Path(__file__).parent
OUT = HERE / "results"
TOLERANCE = 2.0  # px, per edge


def load_rgb(path):
    return np.asarray(Image.open(path).convert("RGB"), dtype=np.float64)


def ssim(a, b):
    """Mean SSIM over 8x8 blocks, the usual window for screenshot comparison."""
    h = min(a.shape[0], b.shape[0])
    w = min(a.shape[1], b.shape[1])
    a = a[:h, :w].mean(axis=2)
    b = b[:h, :w].mean(axis=2)
    if h < 8 or w < 8:
        return 0.0
    c1, c2 = (0.01 * 255) ** 2, (0.03 * 255) ** 2
    total = 0.0
    count = 0
    for y in range(0, h - 7, 8):
        for x in range(0, w - 7, 8):
            pa = a[y:y + 8, x:x + 8]
            pb = b[y:y + 8, x:x + 8]
            mu_a, mu_b = pa.mean(), pb.mean()
            var_a, var_b = pa.var(), pb.var()
            cov = ((pa - mu_a) * (pb - mu_b)).mean()
            num = (2 * mu_a * mu_b + c1) * (2 * cov + c2)
            den = (mu_a ** 2 + mu_b ** 2 + c1) * (var_a + var_b + c2)
            total += num / den
            count += 1
    return total / count


def parent_path(path):
    head, sep, _ = path.rpartition(" > ")
    return head if sep else None


def compare_site(slug, url, ours, chrome):
    elements = ours["elements"]
    rects = chrome["rects"]
    abs_deltas, local_deltas = [], []
    matched = 0
    for element in elements:
        expected = rects.get(element["path"])
        if expected is None:
            continue
        matched += 1
        got = element["rect"]
        abs_deltas.append(
            max(
                abs(got["x"] - expected["x"]),
                abs(got["y"] - expected["y"]),
                abs(got["width"] - expected["w"]),
                abs(got["height"] - expected["h"]),
            )
        )
        # Offset of this box inside its parent, in both engines.
        parent = parent_path(element["path"])
        parent_ours = next((e["rect"] for e in elements if e["path"] == parent), None)
        parent_chrome = rects.get(parent) if parent else None
        if parent_ours is None or parent_chrome is None:
            got_offset = (got["x"], got["y"])
            expected_offset = (expected["x"], expected["y"])
        else:
            got_offset = (got["x"] - parent_ours["x"], got["y"] - parent_ours["y"])
            expected_offset = (expected["x"] - parent_chrome["x"], expected["y"] - parent_chrome["y"])
        local_deltas.append(
            max(
                abs(got_offset[0] - expected_offset[0]),
                abs(got_offset[1] - expected_offset[1]),
                abs(got["width"] - expected["w"]),
                abs(got["height"] - expected["h"]),
            )
        )

    def rate(deltas, limit):
        return sum(1 for d in deltas if d <= limit)

    return {
        "slug": slug,
        "url": url,
        "status": "ok",
        "elements": len(elements),
        "matched": matched,
        "abs_2": rate(abs_deltas, 2.0),
        "abs_8": rate(abs_deltas, 8.0),
        "local_2": rate(local_deltas, 2.0),
        "local_8": rate(local_deltas, 8.0),
        "local_median": float(np.median(local_deltas)) if local_deltas else 0.0,
        "abs_median": float(np.median(abs_deltas)) if abs_deltas else 0.0,
        "abs_worst": float(max(abs_deltas)) if abs_deltas else 0.0,
        "ours_ms": ours.get("elapsed_ms"),
        "chrome_ms": chrome.get("elapsed_ms"),
    }


def main():
    only = set(sys.argv[1:])
    rows = []
    for line in (HERE / "sites.txt").read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        slug, url = line.split("\t")
        if only and slug not in only:
            continue
        paths = {
            "ours_json": OUT / f"{slug}-ours.json",
            "chrome_json": OUT / f"{slug}-chrome.json",
            "ours_png": OUT / f"{slug}-ours.png",
            "chrome_png": OUT / f"{slug}-chrome.png",
        }
        if not all(p.exists() for p in paths.values()):
            rows.append({"slug": slug, "url": url, "status": "missing output"})
            print(f"{slug:<20} missing output")
            continue
        ours = json.loads(paths["ours_json"].read_text())
        chrome = json.loads(paths["chrome_json"].read_text())
        if chrome.get("status") != "ok":
            rows.append({"slug": slug, "url": url, "status": chrome.get("status", "failed")})
            continue

        row = compare_site(slug, url, ours, chrome)
        row["ssim"] = ssim(load_rgb(paths["ours_png"]), load_rgb(paths["chrome_png"]))
        rows.append(row)
        print(
            f"{slug:<20} abs {row['abs_2']:>4}/{row['matched']:<5} "
            f"({row['abs_2']/row['matched']*100 if row['matched'] else 0:5.1f}%)  "
            f"local {row['local_2']:>4}/{row['matched']:<5} "
            f"({row['local_2']/row['matched']*100 if row['matched'] else 0:5.1f}%)  "
            f"median local {row['local_median']:7.1f}px  ssim {row['ssim']:.3f}"
        )

    (HERE / "results.json").write_text(json.dumps(rows, indent=2))

    ok = [r for r in rows if r.get("status") == "ok"]
    if ok:
        matched = sum(r["matched"] for r in ok)
        abs2 = sum(r["abs_2"] for r in ok)
        loc2 = sum(r["local_2"] for r in ok)
        loc8 = sum(r["local_8"] for r in ok)
        mean_ssim = sum(r["ssim"] for r in ok) / len(ok)
        print(
            f"\n{len(ok)} sites · {matched} matched boxes\n"
            f"  absolute within 2px: {abs2} ({abs2/matched*100:.1f}%)\n"
            f"  local    within 2px: {loc2} ({loc2/matched*100:.1f}%)\n"
            f"  local    within 8px: {loc8} ({loc8/matched*100:.1f}%)\n"
            f"  mean SSIM: {mean_ssim:.3f}"
        )
        write_markdown(ok, matched, abs2, loc2, loc8, mean_ssim)


def write_markdown(ok, matched, abs2, loc2, loc8, mean_ssim):
    """The results table, ready to paste into the README."""
    lines = [
        "| site | boxes | within 2px (page) | within 2px (local) | within 8px (local) | median local err | SSIM | ours | Chromium |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for r in ok:
        n = r["matched"] or 1
        lines.append(
            f"| {r['slug']} | {r['matched']} | {r['abs_2']/n*100:.0f}% | {r['local_2']/n*100:.0f}% | "
            f"{r['local_8']/n*100:.0f}% | {r['local_median']:.1f} px | {r['ssim']:.3f} | "
            f"{(r['ours_ms'] or 0):.0f} ms | {(r['chrome_ms'] or 0):.0f} ms |"
        )
    n = matched or 1
    lines.append(
        f"| **all 20** | **{matched}** | **{abs2/n*100:.1f}%** | **{loc2/n*100:.1f}%** | "
        f"**{loc8/n*100:.1f}%** | | **{mean_ssim:.3f}** | | |"
    )
    (HERE / "results.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
