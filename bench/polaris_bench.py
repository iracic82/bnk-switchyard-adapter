#!/usr/bin/env python3
"""The agent workload through BNK's AI gateway (Polaris), three ways: always the small model,
always the large model, and model "auto" (Switchyard decides per step).

Every request carries a Polaris virtual key and goes client -> Polaris -> (Switchyard for "auto")
-> pool bridge -> F5 EPP -> model pod. The served model is read from the response's `model`
field, cost from its `usage` and the per-million-token prices Polaris has for each model.

Simulated backends: success rates, latency and prices come from the mock model servers and the
lab's configured prices, not from GPUs. The judge call Switchyard makes for each "auto" request
is not in the response usage; it is estimated from the classifier prompt size and listed apart.

  python3 bench/polaris_bench.py --url http://172.16.100.19 --key-file /root/.bnk/aigw/switchyard-eng-key.key
"""

import argparse
import json
import os
import statistics
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))
from workload import generate  # noqa: E402

# USD per million tokens (input, output): the prices the lab sets on the Polaris models.
PRICES = {"sy-small": (0.10, 0.40), "sy-large": (2.00, 8.00)}
JUDGE_PROMPT_TOKENS = 1150  # the capability classifier's prompt, about this size per decision


def call(url, key, model, prompt, timeout):
    body = json.dumps({"model": model, "messages": [{"role": "user", "content": prompt}]}).encode()
    req = urllib.request.Request(f"{url}/v1/chat/completions", data=body, method="POST", headers={
        "Authorization": f"Bearer {key}", "Content-Type": "application/json"})
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            doc = json.load(r)
            pod = r.headers.get("x-served-by", "")
            pick = r.headers.get("x-pool-pick", "")
    except urllib.error.HTTPError as e:
        return {"error": f"HTTP {e.code}", "seconds": time.monotonic() - t0}
    except Exception as e:  # timeouts, resets
        return {"error": type(e).__name__, "seconds": time.monotonic() - t0}
    usage = doc.get("usage") or {}
    served = doc.get("model", "")
    pin, pout = PRICES.get(served, (0.0, 0.0))
    text = ((doc.get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    return {"error": None, "seconds": time.monotonic() - t0, "served": served, "pod": pod, "pick": pick,
            "ok": "RESULT: PASS" in text,
            "cost": usage.get("prompt_tokens", 0) * pin / 1e6 + usage.get("completion_tokens", 0) * pout / 1e6}


def run(url, key, model, episodes, args):
    def episode(ep):
        steps = [call(url, key, model, s["prompt"], args.timeout) for s in ep["steps"]]
        return {"steps": steps, "ok": all(s["error"] is None and s["ok"] for s in steps)}
    t0 = time.monotonic()
    with ThreadPoolExecutor(args.concurrency) as pool:
        done = list(pool.map(episode, episodes))
    steps = [s for e in done for s in e["steps"]]
    good = [s for s in steps if s["error"] is None]
    lat = sorted(s["seconds"] for s in good)
    cost = sum(s["cost"] for s in good)
    judge = len(steps) * JUDGE_PROMPT_TOKENS * PRICES["sy-small"][0] / 1e6 if model == "auto" else 0.0
    ok = sum(e["ok"] for e in done)
    return {
        "model": model, "tasks": len(done), "task_success": ok / len(done) if done else 0,
        "steps": len(steps), "errors": len(steps) - len(good),
        "served": {m: sum(s["served"] == m for s in good) for m in sorted({s["served"] for s in good})},
        "picked_by_epp": sum(s["pick"] == "epp" for s in good),
        "pods": len({s["pod"] for s in good if s["pod"]}),
        "cost_usd": cost, "judge_usd": judge,
        "cost_per_successful_task": (cost + judge) / ok if ok else None,
        "p50_s": statistics.median(lat) if lat else None,
        "p95_s": lat[int(0.95 * (len(lat) - 1))] if lat else None,
        "wall_s": time.monotonic() - t0,
    }


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--url", default="http://172.16.100.19")
    p.add_argument("--key-file", default=os.path.join(os.environ.get("SECRETS_DIR", "/root/.bnk"), "aigw/switchyard-eng-key.key"))
    p.add_argument("--models", default="sy-small,sy-large,auto")
    p.add_argument("--episodes", type=int, default=30)
    p.add_argument("--concurrency", type=int, default=8)
    p.add_argument("--seed", type=int, default=7)
    p.add_argument("--timeout", type=float, default=120)
    p.add_argument("--json", help="also write the results here")
    args = p.parse_args()
    key = open(args.key_file).read().strip()
    episodes = generate(args.episodes, args.seed, 0.3)
    print(f"{args.episodes} agent tasks, {sum(len(e['steps']) for e in episodes)} steps each run, through {args.url}")
    print("Simulated backends: success, latency and prices come from mock model servers.\n")
    results = []
    for m in args.models.split(","):
        print(f"  running {m} ...", flush=True)
        results.append(run(args.url, key, m, episodes, args))
    print()
    print(f"{'model':<9} {'task success':>12} {'errors':>6} {'served small/large':>18} {'EPP picks':>9} "
          f"{'cost USD':>9} {'+judge':>7} {'USD/success':>11} {'p50 s':>6} {'p95 s':>6}")
    for r in results:
        sl = f"{r['served'].get('sy-small', 0)}/{r['served'].get('sy-large', 0)}"
        cps = f"{r['cost_per_successful_task']:.5f}" if r["cost_per_successful_task"] else "-"
        print(f"{r['model']:<9} {r['task_success']:>12.0%} {r['errors']:>6} {sl:>18} {r['picked_by_epp']:>9} "
              f"{r['cost_usd']:>9.4f} {r['judge_usd']:>7.4f} {cps:>11} {r['p50_s'] or 0:>6.2f} {r['p95_s'] or 0:>6.2f}")
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=2)


if __name__ == "__main__":
    main()
