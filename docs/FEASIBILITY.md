# Phase 1 feasibility report

Date: 2026-11-05. Targets: NVIDIA NeMo Switchyard at commit
`993d9c5486bfd79f1edfaeda026672c4d9f5d537` (2026-11-04, workspace version 0.3.0) and
F5 BIG-IP Next for Kubernetes (BNK) 2.4.2.

Each item below is marked **verified**, **verified in a lab** or **unverified**.

- **Verified** means we read it in source code or in F5 documentation.
- **Verified in a lab** means a manifest of that exact shape has run on a real BNK 2.4 cluster
  in an earlier lab. The cluster for this project was not available.
- **Unverified** means nothing confirms it yet.

## Switchyard interfaces (verified in source)

| Interface | Where | What we rely on |
|---|---|---|
| `Algorithm::run_stream(self: Arc<Self>, Request, Arc<RuntimeModels>, State) -> StepStream` | `crates/libsy/src/core/algorithm.rs` | The adapter consumes this stream directly. |
| `Step::CallModel(Box<CallModel>)` | same | `call.models` (candidates, never empty), `call.request` (model stamped), `call.respond(future)`, `call.recover_errors`. |
| `Step::CallDecision(Box<CallDecision>)` | same | `call.model`, `call.request`, `call.respond(future)`. |
| `Step::Done(Box<RoutingOutcome>)` | same | `selected_model_ids` (best first), `request`, `fallback_request`, `candidate_requests`, `response` (set when the algorithm already answered), `metadata`. |
| `RuntimeModels::new(HashMap<Category, Vec<ModelId>>).with_target_restriction()` | same | The algorithm can only see and call the models we give it. libsy also rejects any other target. |
| Dropping the `StepStream` aborts the algorithm task (`AbortOnDrop`) | same | Client cancellation stops routing work. |
| `Passthrough`, `Random::new(weights, seed)`, `LlmTaskClassifier::new(LlmClassifierConfig::Capability{..})` | `crates/libsy/src/algorithms/*` | Three initial algorithms. `TaskClassifierConfig` implements `Deserialize`. |
| Task classifier uses categories `judge`, `efficient`, `capable` and calls the judge through `Step::CallModel` with a JSON-schema verdict (`crux`, `primary_rule`, `capability_boundary`, `p_solve`) | `llm_class.rs`, `prompts/capability-classifier/schema.json` | The judge call is an intermediate model call. The adapter serves it, and the mock judge model implements the schema. |
| `switchyard_translation::decode_request(WireFormat::OpenAiChat, &Value)`, `encode_aggregated_response`, `encode_stream`, `prepare_request_for_target` | `crates/switchyard-translation/src/helpers.rs`, `util.rs` | OpenAI Chat in, OpenAI Chat out, both streaming and buffered. Tool calls are part of the IR. |
| `TranslatingLlmClient::new(&[ModelConfig])`, `ModelConfig::new(name, Backend::OpenAiChat(HttpBackendConfig{base_url, extra_headers, timeout, max_retries, ..}), None).with_provider_model(..)` | `crates/libsy-llm-client/src/{client,backend}.rs` | One logical model maps to one base URL plus fixed headers. This is the model-to-pool binding. |
| With `forward_auth = false`, only `x-request-id` is copied from `Metadata::http_headers` to the upstream call | `client.rs` `ALLOWED_METADATA_HEADERS` | The request ID reaches BNK and the model server. The client's `Authorization` header never does. |

Switchyard also ships `libsy-llm-client::run`, a complete driver. We do **not** use it for the
main loop. The task asks for the adapter to own the `run_stream` lifecycle. Owning the loop also
gives us one place to enforce tenant policy on every step and to time each step. We do reuse its
HTTP client and its translation code.

### Switchyard limitations found

1. `Metadata::from_headers` trusts the client's `x-switchyard-*` headers, such as the session ID.
   This is fine for routing hints but not for identity. The adapter never takes tenant identity
   from these headers. It scopes session IDs by tenant, so one tenant cannot reuse another
   tenant's routing state.
2. Only `x-request-id` is forwarded upstream. W3C `traceparent` is not, so the
   adapter's OpenTelemetry trace stops at the adapter. Upstream change proposed in `UPSTREAM.md`.
3. The task classifier's judge is a chat model call. Its cost counts as model-selection cost, and
   the adapter reports it separately.
4. `StageRouter` keeps request history per session (`stage.rs`, `util/stage.rs`). We did not add
   it in this release. Its history is in-process, and running several adapter replicas would need
   session affinity at the external gateway. That affinity is not built or tested.

## BNK interfaces

| Interface | Status | Source |
|---|---|---|
| `F5EPP` (`inference.k8s.f5.com/v1alpha1`) with `spec.poolRef.name`, `spec.engine` (`vllm`, `dynamo`, `sglang`), `spec.server.modelServerMetricsPort`, `spec.tokenizer.name`, `spec.eppConfig` | verified in a lab (BNK 2.4) | F5 EPP how-to page; earlier lab manifests |
| `InferencePool` (`inference.networking.k8s.io/v1`, GAIE v1.4.0) with `selector.matchLabels`, `targetPorts`, `endpointPickerRef{name, port.number: 9002, failureMode}` | verified, verified in a lab | F5 how-to page (Step 2), GAIE CRD |
| `CNEInstance.spec.eppNamespaces` must list the pool namespace | verified | F5 how-to page (Step 5) |
| GatewayClass `f5-gateway-class`, `Gateway.spec.addresses[].value` as the VIP | verified in a lab | earlier lab manifests |
| `HTTPRoute.backendRefs` to `group: inference.networking.k8s.io, kind: InferencePool` | verified | F5 how-to page (GAIE conformance) |
| "Two `HTTPRoute`s on two different Gateways can each route independently to their own `InferencePool`" | verified | F5 how-to page (GAIE conformance) |
| `HTTPRoute` header match (`matches[].headers`) on a BNK Gateway | verified in a lab, with **Service** backends | earlier lab (`X-Route-To: complex`) |
| One `HTTPRoute` with header matches that selects between **two InferencePools** on one Gateway | **unverified** | No document or lab run shows it |
| TMM SNATs hairpinned traffic (known issue 2290393) | verified in a lab | earlier lab notes |
| BNK 2.4.2 changes since 2.4.1 | PHP CVE fixes only; no API changes | release-notes check, 2026-10-31 |

## Can BNK do what the design needs?

**The internal routing path: yes.** F5 documents one InferencePool per Gateway. So the default
manifests use **one internal Gateway per model pool**, each with its own VIP and one HTTPRoute.
The adapter's registry maps each logical model to that Gateway's URL. Nothing here needs an
unverified feature.

The leaner design puts one internal Gateway in front of both pools and uses a header match. The
adapter sends `x-switchyard-pool: <pool>`. This is the pattern GAIE uses for body-based routing.
It ships as an optional overlay (`deploy/k8s/overlays/single-internal-gateway`) and is marked
"needs validation on a BNK cluster". The adapter needs no code change to use it; only the config
changes.

**Loop prevention:** the external and internal listeners are separate Gateways with separate
VIPs. Each HTTPRoute names exactly one parent. As a second guard, the adapter adds
`x-switchyard-adapter-hop: 1` to every upstream call. It refuses any incoming request that already
carries that header, with `508 Loop Detected`. The external route must **not** strip this header.
Stripping it would hide a loop. A client that sets the header itself only causes its own request
to be refused.

**Trust:** because of TMM SNAT (known issue 2290393), the adapter cannot trust a source address.
Tenant identity therefore comes from an API key that the adapter checks against SHA-256 digests
from a Kubernetes Secret (`identity.mode = "api_key"`). A second mode (`trusted_header`) reads a
header set by an authenticating gateway in front of the adapter. It is for use only once that
gateway is verified to remove any client copy of the header.

**Model selection versus pool selection:** a model ID does not select a pool by itself. The
adapter's registry binds each logical model to `{provider_model, base_url, headers}`. The pool
is chosen by which internal Gateway the URL names, or by which header value the route matches.
The EPP then picks the endpoint inside that pool. The adapter never picks an endpoint.

## Verified on a BNK 2.4.2 cluster (2026-11-05)

These were run on a live BNK 2.4.2 lab cluster.

- **Switchyard works as Polaris's model router.** The adapter's ext_proc server, attached with
  `PUT /api/v1/tenants/<t>/profiles/<p>/semantic-router`, is called by BNK's AI gateway (Polaris)
  for each request. Its answer (`model` rewritten to a Polaris AIModel name, `x-selected-model`,
  route cache cleared) is honoured: easy agent steps went to the small model and hard ones to the
  large model, about 0.4 s per decision, streaming included.
- **The InferencePool path works.** Requests to the small pool's Gateway were spread by the F5 EPP
  across both pods (24 of 24 succeeded, 9 and 15 per pod).
- **Polaris does not chain to an InferencePool.** `AIProvider` accepts an endpoint URL only, and
  `AIModel` deployments reference a provider only; neither has an InferencePool or endpoint-picker
  field. A Polaris provider whose URL is another BNK Gateway VIP got a TCP reset, while the same VIP
  worked from outside. The lab's existing Polaris models point at plain Services for the same
  reason. **So in BNK 2.4.2 the AI gateway path (identity, budgets, Switchyard) and the
  InferencePool path (EPP endpoint choice) are two entry points, not one chain.**

## What cannot be verified here

There is no BNK cluster and no GPU on this machine. The following work only on a BNK 2.4.2
cluster:

1. TMM consulting the F5EPP for traffic that comes from the adapter. The adapter reaches the
   internal VIP through TMM.
2. The optional header-match overlay that sends traffic to two InferencePools.
3. EPP endpoint selection metrics.
4. Real TTFT, throughput and GPU numbers. Local benchmarks use mock backends, and every report
   says so.

The local implementation still runs every adapter code path against OpenAI-compatible mock
servers. Kubernetes manifests are checked against the Gateway API and GAIE v1.4.0 CRDs with a
server-side dry run on kind. The F5 CRDs (`F5EPP`, `CNEInstance`) are not public. Our F5EPP
manifests use only fields from the F5 documentation and from earlier lab manifests that ran on
BNK 2.4.
