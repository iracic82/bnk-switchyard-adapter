# Instruqt track: bnk-switchyard-routing

Four challenges on the bnk-ai-demos cluster (three VMs, BNK 2.4 installed by `bnk-deploy`,
the DPU simulated with gretap overlays). `track_scripts/setup-bnk` runs the bnk-ai-demos
setup unchanged, then `scripts/instruqt-layer.sh` from this repository.

| Challenge | Shows | Check |
|---|---|---|
| 01 two-decisions | Switchyard picks the model, BNK's EPP picks the pod | `auto` requests reached both models |
| 02 model-to-pool | Gateway, HTTPRoute, InferencePool, F5EPP chain; load spread across pods | every small-model pod served a request |
| 03 tenant-governance | refusal, degrade, key-based identity, token cap | support tenant refused once and served once |
| 04 cost-of-a-decision | small only vs large only vs Switchyard through BNK | report with a `switchyard-bnk` row |

The layer puts the two pools in the `sim` namespace. bnk-ai-demos sets
`eppNamespaces=[sim]` at install time and copies the F5 pull secret there; patching
`eppNamespaces` after install restarted the controller, and its new pod stayed Pending on
this CPU-tight cluster. VIPs `172.16.201.110` to `.112` are unused by the other demos.

## Before publishing

Nothing here has run on Instruqt yet. Every check was rehearsed on kind except the ones that
need TMM VIPs.

1. Push the images somewhere the sandbox can pull from without credentials:
   `make docker-build`, then tag and push `bnk-switchyard-adapter` and `mock-llm` as
   `<registry>/<name>:0.1.0`.
2. Push this repository where `GH_READ_TOKEN` can read it.
3. Set `SY_IMAGE_REPO` and `SY_REPO` in `config.yml`. The setup refuses to run while they
   still say `REPLACE-ME`.
4. Run the track once end to end in a test sandbox, then `instruqt track push`.

The base install takes as long as bnk-ai-demos does, because it is the same install with
all five of its demos.
