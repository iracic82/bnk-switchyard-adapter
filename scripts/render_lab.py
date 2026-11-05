#!/usr/bin/env python3
"""Renders deploy/k8s/base for a lab where BNK's AI gateway (Polaris) is the front door.

  kubectl kustomize deploy/k8s/base | scripts/render_lab.py --pool-ns aigw-models \
      --bnk-ns f5-bnk --image-repo docker.io/iracic82 --tag 0.2.0 \
      --adapter-config deploy/instruqt/adapter.toml | kubectl apply -f -

Changes against the base: the external Gateway and its route are dropped (Polaris is the
entry point), the adapter runs in the BNK namespace as Service `switchyard-router` with the
ext_proc port, pools go to the EPP-enabled namespace, and images come from a public registry.
"""

import argparse
import sys

import yaml

IMAGES = {"bnk-switchyard-adapter": "bnk-switchyard-adapter", "mock-llm": "bnk-switchyard-mock-llm"}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--pool-ns", required=True)
    p.add_argument("--bnk-ns", required=True)
    p.add_argument("--image-repo", required=True)
    p.add_argument("--tag", required=True)
    p.add_argument("--adapter-config", required=True)
    p.add_argument("--gateway-ns", default=None, help="namespace for the internal Gateways (default --bnk-ns)")
    p.add_argument("--small-vip", default=None)
    p.add_argument("--large-vip", default=None)
    p.add_argument("--gateway-infra", default=None, help="F5BnkGateway name for spec.infrastructure.parametersRef")
    p.add_argument("--kinds", default="", help="comma list: only emit these kinds")
    args = p.parse_args()
    kinds = set(filter(None, args.kinds.split(",")))
    gateway_ns = args.gateway_ns or args.bnk_ns
    vips = {"inference-small": args.small_vip, "inference-large": args.large_vip}
    config = open(args.adapter_config).read().replace("__POOL_NS__", args.pool_ns)
    out = []
    for doc in yaml.safe_load_all(sys.stdin):
        if not doc:
            continue
        kind, meta = doc["kind"], doc["metadata"]
        if meta["name"] == "switchyard-external" or kind == "Namespace":
            continue
        if meta.get("namespace") == "inference":
            meta["namespace"] = args.pool_ns
        if meta.get("namespace") == "switchyard":
            meta["namespace"] = args.bnk_ns
        if kind == "Gateway":
            meta["namespace"] = gateway_ns
            if vips.get(meta["name"]):
                doc["spec"]["addresses"] = [{"type": "IPAddress", "value": vips[meta["name"]]}]
            if args.gateway_infra:
                doc["spec"]["infrastructure"] = {"parametersRef": {"group": "k8s.f5net.com", "kind": "F5BnkGateway", "name": args.gateway_infra}}
            for listener in doc["spec"]["listeners"]:
                sel = listener["allowedRoutes"]["namespaces"]["selector"]["matchLabels"]
                sel["kubernetes.io/metadata.name"] = args.pool_ns
        if kind == "HTTPRoute":
            for ref in doc["spec"]["parentRefs"]:
                ref["namespace"] = gateway_ns
        if kind == "Deployment":
            spec = doc["spec"]["template"]["spec"]
            for c in spec["containers"]:
                name = c["image"].split(":")[0]
                if name in IMAGES:
                    c["image"] = f"{args.image_repo}/{IMAGES[name]}:{args.tag}"
            if meta["name"] == "switchyard-adapter":
                meta["name"] = "switchyard-router"
                spec["containers"][0]["ports"].append({"name": "grpc", "containerPort": 50051})
        if kind == "Service" and meta["name"] == "switchyard-adapter":
            meta["name"] = "switchyard-router"
            doc["spec"]["ports"].append({"name": "grpc", "port": 50051, "targetPort": "grpc", "appProtocol": "grpc"})
        if kind == "ServiceAccount":
            meta["name"] = "switchyard-router"
        if kind == "Deployment" and meta["name"] == "switchyard-router":
            spec["serviceAccountName"] = "switchyard-router"
            doc["spec"]["selector"]["matchLabels"] = {"app": "switchyard-router"}
            doc["spec"]["template"]["metadata"]["labels"]["app"] = "switchyard-router"
            doc["metadata"]["labels"] = {"app": "switchyard-router"}
            for v in spec["volumes"]:
                if "configMap" in v:
                    v["configMap"]["name"] = "switchyard-router-config"
        if kind == "Service" and meta["name"] == "switchyard-router":
            doc["spec"]["selector"] = {"app": "switchyard-router"}
        if kind == "ConfigMap" and "adapter.toml" in doc.get("data", {}):
            meta["name"] = "switchyard-router-config"
            doc["data"]["adapter.toml"] = config
        if kinds and kind not in kinds:
            continue
        out.append(doc)
    yaml.safe_dump_all(out, sys.stdout, sort_keys=False)


if __name__ == "__main__":
    main()
