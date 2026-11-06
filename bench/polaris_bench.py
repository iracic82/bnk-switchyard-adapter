#!/usr/bin/env python3
"""The agent workload through BNK's AI gateway (Polaris), three ways: always the small model,
always the large model, and model "auto" (Switchyard decides per step).

Every request carries a Polaris virtual key and goes client -> Polaris -> (Switchyard for "auto")
-> pool bridge -> F5 EPP -> model pod. The served model is read from the response's `model`
field, cost from its `usage` and the per-million-token prices Polaris has for each model.

Simulated backends, not GPUs: the small pool is mock-llm, the large pool llm-d's vLLM simulator.
Latency and token counts come from them, prices from the lab's Polaris models. Task success is
mock-llm's verdict where it gives one; for other servers the bench draws it with the same rates
(bench/workload.py, step_outcome) and counts how often it did. The judge call Switchyard makes for each "auto" request
is not in the response usage; it is estimated from the classifier prompt size and listed apart.

  python3 bench/polaris_bench.py --url http://172.16.100.19 --key-file /root/.bnk/aigw/switchyard-eng-key.key

--sweep 0.3,0.5,0.7 runs "auto" once per judge threshold (the lab's `sy threshold` sets it and restarts
the router), after one run each of sy-small and sy-large, and puts the original threshold back.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))
from workload import generate, step_outcome  # noqa: E402

# USD per million tokens (input, output): the prices the lab sets on the Polaris models.
PRICES = {"sy-small": (0.10, 0.40), "sy-large": (2.00, 8.00), "sy-dynamo": (2.00, 8.00)}
LARGE = {"sy-large", "sy-dynamo"}
JUDGE_PROMPT_TOKENS = 1150  # the capability classifier's prompt, about this size per decision


def call(url, key, model, step, timeout):
    body = json.dumps({"model": model, "messages": [{"role": "user", "content": step["prompt"]}]}).encode()
    req = urllib.request.Request(f"{url}/v1/chat/completions", data=body, method="POST", headers={
        "Authorization": f"Bearer {key}", "Content-Type": "application/json"})
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            doc = json.load(r)
            # x-served-by comes from mock-llm only; the bridge's endpoint header is always set.
            pod = r.headers.get("x-served-by") or r.headers.get("x-pool-endpoint", "")
            pick = r.headers.get("x-pool-pick", "")
    except urllib.error.HTTPError as e:
        return {"error": f"HTTP {e.code}", "seconds": time.monotonic() - t0}
    except Exception as e:  # timeouts, resets
        return {"error": type(e).__name__, "seconds": time.monotonic() - t0}
    usage = doc.get("usage") or {}
    served = doc.get("model", "")
    pin, pout = PRICES.get(served, (0.0, 0.0))
    text = ((doc.get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    ok, simulated = step_outcome(text, step, served in LARGE, served)
    return {"error": None, "seconds": time.monotonic() - t0, "served": served, "pod": pod, "pick": pick,
            "ok": ok, "simulated": simulated,
            "cost": usage.get("prompt_tokens", 0) * pin / 1e6 + usage.get("completion_tokens", 0) * pout / 1e6}


def run(url, key, model, episodes, args):
    def episode(ep):
        steps = [call(url, key, model, s, args.timeout) for s in ep["steps"]]
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
        "outcomes_simulated_by_bench": sum(s["simulated"] for s in good),
        "pods": len({s["pod"] for s in good if s["pod"]}),
        "cost_usd": cost, "judge_usd": judge,
        "cost_per_successful_task": (cost + judge) / ok if ok else None,
        "p50_s": statistics.median(lat) if lat else None,
        "p95_s": lat[int(0.95 * (len(lat) - 1))] if lat else None,
        "wall_s": time.monotonic() - t0,
    }


def set_threshold(value):
    out = subprocess.run(["sy", "threshold", str(value)], capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"sy threshold {value} failed: {out.stdout}{out.stderr}")
    return out.stdout.strip()


def current_threshold():
    out = subprocess.run(["sy", "threshold"], capture_output=True, text=True).stdout
    return out.strip().rsplit(" ", 1)[-1] if out.strip() else None


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--url", default="http://172.16.100.19")
    keys = os.path.join(os.environ.get("SECRETS_DIR", "/root/.bnk"), "aigw")
    # the dynamo key's scope has all three models; without the Dynamo pool, the engineering key
    dk = os.path.join(keys, "switchyard-dynamo-key.key")
    p.add_argument("--key-file", default=dk if os.path.exists(dk) else os.path.join(keys, "switchyard-eng-key.key"))
    p.add_argument("--models", default="sy-small,sy-large,auto")
    p.add_argument("--episodes", type=int, default=30)
    p.add_argument("--concurrency", type=int, default=8)
    p.add_argument("--seed", type=int, default=7)
    p.add_argument("--timeout", type=float, default=120)
    p.add_argument("--json", help="also write the results here")
    p.add_argument("--sweep", help="judge thresholds to run auto at, e.g. 0.3,0.5,0.7,0.9")
    args = p.parse_args()
    key = open(args.key_file).read().strip()
    episodes = generate(args.episodes, args.seed, 0.3)
    print(f"{args.episodes} agent tasks, {sum(len(e['steps']) for e in episodes)} steps each run, through {args.url}")
    print("Simulated backends: small pool mock-llm, large pool llm-d's vLLM simulator. Task success for")
    print("steps the simulator served is drawn by this bench at mock-llm's rates.\n")
    results = []
    if args.sweep:
        before = current_threshold()
        for m in ("sy-small", "sy-large"):
            print(f"  running {m} ...", flush=True)
            results.append(run(args.url, key, m, episodes, args))
        try:
            for t in args.sweep.split(","):
                print(f"  threshold {t}: {set_threshold(t)}", flush=True)
                r = run(args.url, key, "auto", episodes, args)
                r["model"], r["threshold"] = f"auto@{t}", float(t)
                results.append(r)
        finally:
            if before:
                print(f"  threshold back to {before}: {set_threshold(before)}", flush=True)
    else:
        for m in args.models.split(","):
            print(f"  running {m} ...", flush=True)
            results.append(run(args.url, key, m, episodes, args))
    print()
    print(f"{'model':<10} {'task success':>12} {'errors':>6} {'small/large':>11} {'to large':>8} {'EPP picks':>9} "
          f"{'cost USD':>9} {'+judge':>7} {'USD/success':>11} {'p50 s':>6} {'p95 s':>6}")
    for r in results:
        small, large = r["served"].get("sy-small", 0), r["served"].get("sy-large", 0) + r["served"].get("sy-dynamo", 0)
        r["to_large"] = large / (small + large) if small + large else 0.0
        cps = f"{r['cost_per_successful_task']:.5f}" if r["cost_per_successful_task"] else "-"
        print(f"{r['model']:<10} {r['task_success']:>12.0%} {r['errors']:>6} {f'{small}/{large}':>11} {r['to_large']:>8.0%} "
              f"{r['picked_by_epp']:>9} {r['cost_usd']:>9.4f} {r['judge_usd']:>7.4f} {cps:>11} "
              f"{r['p50_s'] or 0:>6.2f} {r['p95_s'] or 0:>6.2f}")
    print(f"\noutcomes drawn by the bench (no RESULT line in the answer): "
          + ", ".join(f"{r['model']} {r['outcomes_simulated_by_bench']}/{r['steps']}" for r in results))
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=2)


if __name__ == "__main__":
    main()
