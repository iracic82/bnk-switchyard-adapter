#!/usr/bin/env python3
"""Rewrites the rendered base manifests for an existing cluster layout.

  kubectl kustomize deploy/k8s/base | scripts/retarget.py --pool-namespace sim \
      --image-repo ghcr.io/example --tag 0.1.0 --vip-prefix 172.16.201 | kubectl apply -f -

--pool-namespace moves the model servers, InferencePools, F5EPPs and internal HTTPRoutes
into a namespace that is already in CNEInstance.spec.eppNamespaces, so the CNEInstance
does not need a patch. It also updates the internal Gateways' allowedRoutes and the
adapter config's pool names.
"""

import argparse
import re
import sys

import yaml


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--pool-namespace", default="inference")
    p.add_argument("--image-repo", default="")
    p.add_argument("--tag", default="dev")
    p.add_argument("--vip-prefix", default="172.16.201")
    p.add_argument("--kinds", default="", help="comma list: only emit these kinds")
    args = p.parse_args()
    ns = args.pool_namespace
    kinds = set(filter(None, args.kinds.split(",")))
    out = []
    for doc in yaml.safe_load_all(sys.stdin):
        if not doc:
            continue
        kind, meta = doc["kind"], doc["metadata"]
        if kind == "Namespace" and meta["name"] == "inference" and ns != "inference":
            continue
        if meta.get("namespace") == "inference":
            meta["namespace"] = ns
        if kind == "Gateway":
            for listener in doc["spec"]["listeners"]:
                sel = listener.get("allowedRoutes", {}).get("namespaces", {}).get("selector", {}).get("matchLabels", {})
                if sel.get("kubernetes.io/metadata.name") == "inference":
                    sel["kubernetes.io/metadata.name"] = ns
            for address in doc["spec"].get("addresses", []):
                address["value"] = re.sub(r"^172\.16\.201", args.vip_prefix, address["value"])
        if kind == "Deployment":
            for c in doc["spec"]["template"]["spec"]["containers"]:
                name, _, _ = c["image"].partition(":")
                if args.image_repo and name in ("bnk-switchyard-adapter", "mock-llm"):
                    c["image"] = f"{args.image_repo}/{name}:{args.tag}"
                    c["imagePullPolicy"] = "IfNotPresent"
        if kind == "ConfigMap" and "adapter.toml" in doc.get("data", {}):
            text = doc["data"]["adapter.toml"]
            text = text.replace('inference_pool = "inference/', f'inference_pool = "{ns}/')
            text = text.replace("http://172.16.201.", f"http://{args.vip_prefix}.")
            doc["data"]["adapter.toml"] = text
        if kinds and kind not in kinds:
            continue
        out.append(doc)
    yaml.safe_dump_all(out, sys.stdout, sort_keys=False)


if __name__ == "__main__":
    main()
