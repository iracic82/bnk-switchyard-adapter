# Instruqt track: bnk-switchyard-routing

Five challenges on one VM (`lab`, image `f5/aibnk`, n2-standard-32). `track_scripts/setup-lab`
runs the base install of bnk-ai-factory-clab at a pinned commit (fabric, kind, BNK 2.4.2 with
the AI gateway), then two layers from this repository:

- `scripts/instruqt-kind-layer.sh`: the small and large pools with their F5 pickers, the
  Switchyard router (model router and pool bridge), and the Polaris tenant `switchyard` with
  models `sy-small` and `sy-large`, two keys and a route on 172.16.100.19.
- `scripts/instruqt-dynamo-layer.sh`: NVIDIA Dynamo with NATS, two Mocker workers, the fake
  GPU operator and KAI, F5's picker with `engine: dynamo`, and model `sy-dynamo`. Its first
  stage runs while BNK installs. If it fails, the lab starts without challenge 5's pool.

| Challenge | Shows | Check |
|---|---|---|
| 01 two-decisions | Switchyard picks the model, F5's picker picks the pod | `auto` reached both models, a pick by the picker |
| 02 model-to-pool | gateway, provider, pool bridge, InferencePool, pods; load spread | two or more small-pool pods picked |
| 03 tenant-governance | model scope, the pilot key's token budget | the pilot key is refused |
| 04 cost-of-a-decision | sy-small vs sy-large vs auto through the gateway | `/root/bench.json` with three rows |
| 05 dynamo-pool | Dynamo behind F5's picker, `auto-dynamo` | a step routed to `sy-dynamo`, a picker choice in the Dynamo pool |

Model servers: `mock-llm` (small pool, also the judge), llm-d's vLLM simulator (large pool),
Dynamo's Mocker (Dynamo pool). None is a real model.

Setup reads this repository's `main` when the lab starts, so a push to `main` changes the next
play, not a running one. Images come from Docker Hub (`SY_IMAGE_REPO`, `SY_IMAGE_TAG` in
`config.yml`).
