#!/usr/bin/env python3
"""Benchmark harness: the same agent workload under several routing strategies.

Scenarios (the `model` each request sends):
  fixed-small       every step to the small model
  fixed-large       every step to the large model
  switchyard        Switchyard task classifier (route `auto`), endpoint choice by the target
  switchyard-bnk    the same through BNK with F5 EPP endpoint selection (needs --bnk-url)

Run locally, the target is the adapter in front of mock servers, so "switchyard" has no
GPU-aware endpoint selection, and every number is a property of the mocks: latency profiles,
success probabilities and prices are configured, not measured on GPUs. Use the results to
compare routing strategies under stated assumptions, never as production performance.

Standard library only.
"""

import argparse
import concurrent.futures as cf
import datetime as dt
import json
import os
import statistics
import sys
import threading
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import workload  # noqa: E402

TENANTS = {"team-a": "sk-bench-team-a", "team-b": "sk-bench-team-b"}


def percentile(values, p):
    if not values:
        return None
    values = sorted(values)
    k = (len(values) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(values) - 1)
    return values[lo] + (values[hi] - values[lo]) * (k - lo)


def jain(values):
    values = [v for v in values if v is not None]
    if not values or sum(values) == 0:
        return None
    return sum(values) ** 2 / (len(values) * sum(v * v for v in values))


def scrape(url):
    """Sum of adapter counters we diff per scenario."""
    try:
        text = urllib.request.urlopen(f"{url}/metrics", timeout=10).read().decode()
    except (urllib.error.URLError, OSError):
        return None
    totals = {}
    for line in text.splitlines():
        if line.startswith("#") or " " not in line:
            continue
        name_labels, value = line.rsplit(" ", 1)
        name = name_labels.split("{", 1)[0]
        if name == "sy_adapter_estimated_cost_usd_total":
            phase = "selection" if 'phase="selection"' in name_labels else "inference"
            totals[f"cost_{phase}"] = totals.get(f"cost_{phase}", 0.0) + float(value)
        elif name == "sy_adapter_fallbacks_total":
            totals["fallbacks"] = totals.get("fallbacks", 0.0) + float(value)
    return totals


def stream_call(url, key, model, messages, session, timeout):
    body = json.dumps({"model": model, "stream": True, "stream_options": {"include_usage": True},
                       "messages": messages, "max_tokens": 1024}).encode()
    req = urllib.request.Request(f"{url}/v1/chat/completions", data=body, method="POST", headers={
        "authorization": f"Bearer {key}", "content-type": "application/json",
        "x-switchyard-session-id": session})
    start = time.perf_counter()
    ttft = None
    text, usage, served, status, error = [], None, None, 0, None
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status = resp.status
            served = resp.headers.get("x-switchyard-model")
            for raw in resp:
                line = raw.decode().strip()
                if not line.startswith("data: ") or line == "data: [DONE]":
                    continue
                event = json.loads(line[6:])
                if "error" in event:
                    error = event["error"].get("code", "stream_error")
                    break
                if event.get("usage"):
                    usage = event["usage"]
                for choice in event.get("choices", []):
                    piece = choice.get("delta", {}).get("content")
                    if piece:
                        if ttft is None:
                            ttft = time.perf_counter() - start
                        text.append(piece)
    except urllib.error.HTTPError as err:
        status, error = err.code, f"http_{err.code}"
    except (urllib.error.URLError, OSError, TimeoutError) as err:
        error = type(err).__name__
    return {"status": status, "error": error, "ttft": ttft, "e2e": time.perf_counter() - start,
            "text": "".join(text), "usage": usage or {}, "served": served}


def run_episode(url, model, tenant, episode, timeout):
    messages, results = [], []
    session = f"{model}-{episode['episode']}"
    for step in episode["steps"]:
        messages.append({"role": "user", "content": step["prompt"]})
        r = stream_call(url, TENANTS[tenant], model, messages, session, timeout)
        r["ok"] = r["error"] is None and "RESULT: PASS" in r["text"]
        r["hard"] = step["hard"]
        results.append(r)
        if r["error"]:
            break
        # Keep the conversation short: agents carry results forward, not whole transcripts.
        messages.append({"role": "assistant", "content": r["text"][:200]})
        messages = messages[-6:]
    return {"tenant": tenant, "steps": results,
            "ok": len(results) == len(episode["steps"]) and all(s["ok"] for s in results)}


def run_scenario(name, url, model, episodes, args):
    # team-a sends four episodes for every one from team-b: a heavy and a light tenant.
    plan = [("team-b" if i % 5 == 4 else "team-a", ep) for i, ep in enumerate(episodes)]
    before = scrape(args.metrics_url)
    start = time.perf_counter()
    done = []
    lock = threading.Lock()
    with cf.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [pool.submit(run_episode, url, model, tenant, ep, args.timeout) for tenant, ep in plan]
        for future in cf.as_completed(futures):
            with lock:
                done.append(future.result())
    duration = time.perf_counter() - start
    after = scrape(args.metrics_url)
    return summarize(name, model, done, duration, before, after, args)


def summarize(name, model, episodes, duration, before, after, args):
    steps = [s for e in episodes for s in e["steps"]]
    ok_eps = sum(e["ok"] for e in episodes)
    ttfts = [s["ttft"] for s in steps if s["ttft"] is not None]
    e2es = [s["e2e"] for s in steps if s["error"] is None]
    out_tokens = sum(s["usage"].get("completion_tokens", 0) for s in steps)
    served = {}
    for s in steps:
        served[s["served"] or "none"] = served.get(s["served"] or "none", 0) + 1
    cost = None
    if before is not None and after is not None:
        sel = after.get("cost_selection", 0) - before.get("cost_selection", 0)
        inf = after.get("cost_inference", 0) - before.get("cost_inference", 0)
        cost = {"selection_usd": sel, "inference_usd": inf, "total_usd": sel + inf,
                "fallbacks": after.get("fallbacks", 0) - before.get("fallbacks", 0)}
    pools = {"fixed-small": ["small"], "fixed-large": ["large"]}.get(name, ["small", "large"])
    gpus = sum(args.gpus[p] for p in pools)
    per_tenant = {}
    for tenant in TENANTS:
        mine = [e for e in episodes if e["tenant"] == tenant]
        tsteps = [s for e in mine for s in e["steps"] if s["error"] is None]
        per_tenant[tenant] = {
            "episodes": len(mine),
            "success_rate": (sum(e["ok"] for e in mine) / len(mine)) if mine else None,
            "e2e_p95_s": percentile([s["e2e"] for s in tsteps], 95),
            "ttft_p95_s": percentile([s["ttft"] for s in tsteps if s["ttft"] is not None], 95),
        }
    return {
        "scenario": name, "model": model, "episodes": len(episodes), "steps": len(steps),
        "errors": sum(1 for s in steps if s["error"]),
        "task_success_rate": ok_eps / len(episodes) if episodes else None,
        "step_success_rate": sum(s["ok"] for s in steps) / len(steps) if steps else None,
        "served_by_model": served,
        "hard_steps_on_large": sum(1 for s in steps if s["hard"] and s["served"] == "large"),
        "hard_steps": sum(1 for s in steps if s["hard"]),
        "easy_steps_on_large": sum(1 for s in steps if not s["hard"] and s["served"] == "large"),
        "cost": cost,
        "cost_per_successful_task_usd": (cost["total_usd"] / ok_eps) if cost and ok_eps else None,
        "ttft_s": {p: percentile(ttfts, p) for p in (50, 95, 99)},
        "e2e_s": {p: percentile(e2es, p) for p in (50, 95, 99)},
        "duration_s": duration,
        "requests_per_s": len(steps) / duration,
        "output_tokens_per_s": out_tokens / duration,
        "provisioned_gpus_assumed": gpus,
        "successful_tasks_per_provisioned_gpu_hour": ok_eps / (duration / 3600) / gpus if gpus else None,
        "gpu_utilization": "n/a: no GPU telemetry (mock backends)",
        "tenants": per_tenant,
        "tenant_fairness_jain_success": jain([t["success_rate"] for t in per_tenant.values()]),
        "tenant_fairness_jain_inverse_p95": jain([1 / t["e2e_p95_s"] for t in per_tenant.values() if t["e2e_p95_s"]]),
    }


def fmt(v, digits=3):
    if v is None:
        return "n/a"
    if isinstance(v, float):
        return f"{v:.{digits}f}"
    return str(v)


def report(results, args, path):
    lines = [
        "# Benchmark report", "",
        f"Date: {dt.datetime.now().isoformat(timespec='seconds')}. Target: `{args.url}`. "
        f"Episodes per scenario: {args.episodes}, concurrency {args.concurrency}, seed {args.seed}.", "",
        "**Simulated backends.** Latency, success probabilities and prices come from the mock "
        "model servers and the adapter config. These numbers compare routing strategies under "
        "those assumptions. They are not GPU or production measurements. GPU-hours are "
        f"*provisioned* GPUs assumed per pool ({args.gpus}), not measured utilization.", "",
        "| Scenario | Task success | Step success | Total cost (USD) | Cost / successful task | TTFT p50 / p95 / p99 (s) | E2E p50 / p95 (s) | Req/s | Out tok/s | Tasks per prov. GPU-h | Fairness (Jain, success) |",
        "|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for r in results:
        if r.get("skipped"):
            lines.append(f"| {r['scenario']} | not run: {r['skipped']} | | | | | | | | | |")
            continue
        c = r["cost"] or {}
        lines.append(
            f"| {r['scenario']} | {fmt(r['task_success_rate'])} | {fmt(r['step_success_rate'])} | "
            f"{fmt(c.get('total_usd'), 4)} | {fmt(r['cost_per_successful_task_usd'], 5)} | "
            f"{fmt(r['ttft_s'][50], 2)} / {fmt(r['ttft_s'][95], 2)} / {fmt(r['ttft_s'][99], 2)} | "
            f"{fmt(r['e2e_s'][50], 2)} / {fmt(r['e2e_s'][95], 2)} | {fmt(r['requests_per_s'], 1)} | "
            f"{fmt(r['output_tokens_per_s'], 0)} | {fmt(r['successful_tasks_per_provisioned_gpu_hour'], 1)} | "
            f"{fmt(r['tenant_fairness_jain_success'])} |")
    lines += ["", "## Routing detail", "",
              "| Scenario | Served by model | Hard steps on large | Easy steps on large | Selection cost (USD) | Fallbacks | Errors |",
              "|---|---|---|---|---|---|---|"]
    for r in results:
        if r.get("skipped"):
            continue
        c = r["cost"] or {}
        lines.append(f"| {r['scenario']} | {r['served_by_model']} | {r['hard_steps_on_large']}/{r['hard_steps']} | "
                     f"{r['easy_steps_on_large']} | {fmt(c.get('selection_usd'), 5)} | {fmt(c.get('fallbacks'), 0)} | {r['errors']} |")
    lines += ["", "## Tenants", "", "| Scenario | Tenant | Episodes | Success | E2E p95 (s) | TTFT p95 (s) |", "|---|---|---|---|---|---|"]
    for r in results:
        for name, t in (r.get("tenants") or {}).items():
            lines.append(f"| {r['scenario']} | {name} | {t['episodes']} | {fmt(t['success_rate'])} | {fmt(t['e2e_p95_s'], 2)} | {fmt(t['ttft_p95_s'], 2)} |")
    lines += ["", "GPU utilization: " + (results[0].get("gpu_utilization", "n/a") if results else "n/a") + "."]
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", default=os.environ.get("ADAPTER_URL", "http://127.0.0.1:9480"))
    parser.add_argument("--bnk-url", default=os.environ.get("BNK_URL"),
                        help="external BNK VIP in front of the adapter, for the switchyard-bnk scenario")
    parser.add_argument("--metrics-url", default=None,
                        help="adapter URL for /metrics (cost source); defaults to --url")
    parser.add_argument("--episodes", type=int, default=60)
    parser.add_argument("--concurrency", type=int, default=16)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--gpus-small", type=int, default=1)
    parser.add_argument("--gpus-large", type=int, default=4)
    parser.add_argument("--scenarios", default="fixed-small,fixed-large,switchyard,switchyard-bnk")
    parser.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "results"))
    args = parser.parse_args()
    args.gpus = {"small": args.gpus_small, "large": args.gpus_large}
    args.metrics_url = args.metrics_url or args.url

    episodes = workload.generate(args.episodes, args.seed, 0.3)
    models = {"fixed-small": "small", "fixed-large": "large", "switchyard": "auto", "switchyard-bnk": "auto"}
    results = []
    for name in args.scenarios.split(","):
        if name == "switchyard-bnk":
            if not args.bnk_url:
                results.append({"scenario": name, "skipped": "needs a BNK cluster (--bnk-url)"})
                continue
            url = args.bnk_url
        else:
            url = args.url
        print(f"running {name} ...", file=sys.stderr)
        results.append(run_scenario(name, url, models[name], episodes, args))
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    out = os.path.join(args.out, stamp)
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "results.json"), "w") as f:
        json.dump({"args": {k: v for k, v in vars(args).items()}, "results": results}, f, indent=2)
    report(results, args, os.path.join(out, "report.md"))
    print(os.path.join(out, "report.md"))


if __name__ == "__main__":
    main()
