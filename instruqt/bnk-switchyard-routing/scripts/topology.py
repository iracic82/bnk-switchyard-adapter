#!/usr/bin/env python3
"""The Topology tab: the request path of this lab, drawn, with live requests moving along it.

    /topology        the page (topology.html next to this file)
    /api/state       pools, pods (name, IP, node), pickers, and the GPUs the nodes report; kubectl, cached 5 s
    /api/events      server-sent events from the router's log: "ext_proc routed" (Switchyard chose the
                     model) and "pool bridge forwarded" (F5's picker chose the pod)
    /api/send        POST {"kind": ...}: sends one of a few fixed requests with the lab's `sy` helper

Standard library only. The page shows what the learner can also see with kubectl and the router's log.

    python3 topology.py --bind 0.0.0.0 --port 8092
"""
import argparse
import json
import os
import queue
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
ENV = dict(os.environ, KUBECONFIG=os.environ.get("KUBECONFIG", "/root/lab/clab/kubeconfig"),
           SECRETS_DIR=os.environ.get("SECRETS_DIR", "/root/.bnk"),
           PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
POOL_NS, BNK_NS = "aigw-models", "f5-bnk"
POOLS = {  # pool -> (pod selector, picker, what serves it)
    "small": ("app=small-model", "small-pool-epp", "mock-llm (also the judge)"),
    "large": ("app=large-model", "large-pool-epp", "llm-d vLLM simulator"),
    "dynamo": ("nvidia.com/dynamo-graph-deployment-name=mocker,nvidia.com/dynamo-component-type=decode",
               "model-pool-epp", "NVIDIA Dynamo Mocker"),
}
# Fixed requests only: the page cannot send its own text.
SENDS = {
    "easy": ["ask", "auto", "Translate 'good morning' into Spanish."],
    "hard": ["ask", "auto", "Find the root cause of this distributed deadlock between the queue consumer and the lock service."],
    "dynamo": ["ask", "auto-dynamo", "Design a rate limiter for 50,000 requests per second across three regions and explain the trade-offs."],
    "burst": ["burst", "8", "sy-small", "Summarize this support ticket in one sentence."],
}


def sh(args, timeout=15):
    try:
        return subprocess.run(args, capture_output=True, text=True, timeout=timeout, env=ENV).stdout
    except Exception:
        return ""


def kjson(*args):
    try:
        return json.loads(sh(["kubectl", *args, "-o", "json"]) or "{}")
    except ValueError:
        return {}


_state = {"at": 0.0, "data": None}
_state_lock = threading.Lock()


def state():
    with _state_lock:
        if _state["data"] is not None and time.time() - _state["at"] < 5:
            return _state["data"]
    nodes = {}
    for n in kjson("get", "nodes").get("items", []):
        st, lb = n.get("status", {}), n["metadata"].get("labels", {})
        nodes[n["metadata"]["name"]] = {"gpus": st.get("allocatable", {}).get("nvidia.com/gpu", "0"),
                                         "gpu_product": lb.get("nvidia.com/gpu.product", "")}
    pools = {}
    for pool, (sel, epp, server) in POOLS.items():
        pods = []
        for p in kjson("-n", POOL_NS, "get", "pods", "-l", sel).get("items", []):
            ready = any(c.get("type") == "Ready" and c.get("status") == "True" for c in p.get("status", {}).get("conditions", []))
            pods.append({"name": p["metadata"]["name"], "ip": p.get("status", {}).get("podIP", ""),
                         "node": p.get("spec", {}).get("nodeName", ""), "ready": ready})
        if not pods:
            continue
        e = kjson("-n", POOL_NS, "get", "f5-epps", epp)
        pools[pool] = {"server": server, "picker": epp, "engine": e.get("spec", {}).get("engine", ""),
                       "pods": sorted(pods, key=lambda x: x["name"])}
    data = {"nodes": nodes, "pools": pools, "route_vip": "172.16.100.19", "at": int(time.time())}
    with _state_lock:
        _state.update(at=time.time(), data=data)
    return data


class Hub:
    """Fans router log events out to every open page."""

    def __init__(self):
        self.clients, self.lock = [], threading.Lock()

    def publish(self, event):
        with self.lock:
            for q in list(self.clients):
                try:
                    q.put_nowait(event)
                except queue.Full:
                    pass

    def subscribe(self):
        q = queue.Queue(maxsize=200)
        with self.lock:
            self.clients.append(q)
        return q

    def drop(self, q):
        with self.lock:
            if q in self.clients:
                self.clients.remove(q)


HUB = Hub()


def tail_router():
    """Follows the router's log, restarting when the pod changes."""
    while True:
        p = subprocess.Popen(["kubectl", "-n", BNK_NS, "logs", "-f", "--since=1s", "deploy/switchyard-router"],
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=ENV)
        for line in p.stdout:
            try:
                f = json.loads(line).get("fields", {})
            except ValueError:
                continue
            msg = f.get("message")
            if msg == "ext_proc routed":
                HUB.publish({"kind": "model", "model": f.get("selected_model"), "gateway_model": f.get("gateway_model"),
                             "decision_ms": f.get("decision_ms")})
            elif msg == "pool bridge forwarded" and f.get("pick") != "service":  # not the gateway's health checks
                HUB.publish({"kind": "pod", "pool": f.get("pool"), "pick": f.get("pick"), "endpoint": f.get("endpoint"),
                             "status": f.get("status"), "pick_ms": f.get("pick_ms"), "upstream_ms": f.get("upstream_ms")})
        p.wait()
        time.sleep(2)


_send_lock = threading.Lock()


def send(kind):
    if not _send_lock.acquire(blocking=False):
        return False
    def run():
        try:
            sh(["sy", *SENDS[kind]], timeout=180)
        finally:
            _send_lock.release()
    threading.Thread(target=run, daemon=True).start()
    return True


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0].split("#")[0]
        if path in ("/", "/topology"):
            with open(os.path.join(HERE, "topology.html"), "rb") as f:
                return self.reply(200, f.read(), "text/html; charset=utf-8")
        if path == "/api/state":
            return self.reply(200, state())
        if path == "/api/events":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            q = HUB.subscribe()
            try:
                while True:
                    try:
                        ev = q.get(timeout=15)
                        self.wfile.write(f"data: {json.dumps(ev)}\n\n".encode())
                    except queue.Empty:
                        self.wfile.write(b": keep-alive\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
            finally:
                HUB.drop(q)
            return
        self.reply(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/api/send":
            return self.reply(404, {"error": "not found"})
        try:
            n = min(int(self.headers.get("Content-Length", "0")), 1024)
            kind = json.loads(self.rfile.read(n) or b"{}").get("kind")
        except ValueError:
            kind = None
        if kind not in SENDS:
            return self.reply(400, {"error": "unknown request"})
        if not send(kind):
            return self.reply(429, {"error": "a request is still running"})
        self.reply(202, {"sent": kind})


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bind", default="0.0.0.0")
    ap.add_argument("--port", type=int, default=8092)
    a = ap.parse_args()
    threading.Thread(target=tail_router, daemon=True).start()
    ThreadingHTTPServer((a.bind, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
