# Production gaps and upstream assessment

## Gaps before production

These are what I would fix before anyone runs this for real traffic, in order.

1. **Limits are per replica.** Rate, concurrency and spend limits live in process. With more
   than one adapter replica a tenant gets a multiple of its limit. Either run one replica
   behind BNK (the adapter is light: about 60 MB image, routing overhead is the judge call),
   or move the counters to a shared store. BNK's own rate limiting at the external Gateway is
   worth checking first, because it would keep the adapter stateless.
2. **API keys in a ConfigMap.** The digests are not secrets, but key rotation means editing
   config and restarting. A JWT validated at the external Gateway, with the adapter in
   `trusted_header` mode, is cleaner. That mode is only safe once the Gateway is shown to
   strip any client copy of the header, which has not been tested on BNK.
3. **Spend is an estimate.** It uses configured prices and the token counts the model server
   reports. It is not a GPU quota. A tenant that sends long prompts with tiny outputs costs
   GPU time this does not price. GPU-seconds per request would need per-request timing from
   the engine.
4. **The judge sits on the hot path.** The classifier prompt is about 1,150 tokens and adds
   one model call before every `auto` request (about 0.4 s on the mocks). The classifier's
   `classify_trigger` can re-judge only on new sessions or user turns; for agents that is
   usually the right setting. Measure it on real hardware before tuning anything else.
5. **No TLS on internal hops.** The adapter to internal VIP traffic is plain HTTP. The EPP
   manifests also set `secureServing: false` and `metricsEndpointAuth: false`, as in the lab
   they came from. Turn all of these on once the data path works.
6. **No NetworkPolicy.** TMM reaches pods from its own interfaces and SNATs hairpinned
   traffic, so a namespace selector policy may not match what you expect. Write the policy
   after observing the real source addresses on the cluster.
7. **StageRouter and decision services are left out.** See the README.

## Validation still owed on a BNK cluster

1. Apply `deploy/k8s/base` on BNK 2.4.2 and run `scripts/demo.sh` against the external VIP.
2. Confirm that `x-served-by` moves between pods under load (the EPP is choosing).
3. Run `make benchmark BNK_URL=http://<external VIP> METRICS_URL=<adapter metrics URL>` for
   the `switchyard-bnk` scenario.
4. Try `deploy/k8s/overlays/single-internal-gateway`. If BNK routes by header to two
   InferencePools, it saves two VIPs and two Gateways.
5. Repeat with real vLLM on GPUs to get real TTFT and KV-cache behaviour.

## What could go upstream to Switchyard

Switchyard's own guidance asks for the smallest direct change, one focused commit each, DCO
sign-off, and examples kept out of the core. Ranked by value:

1. **Forward W3C trace context.** `libsy-llm-client` forwards only `x-request-id` from
   request metadata (`ALLOWED_METADATA_HEADERS` in `client.rs`). Adding `traceparent` and
   `tracestate` would let a trace run from the adapter into the inference server, and through
   BNK if it propagates them. Small, self-contained, testable with the existing wiremock tests.
2. **An example under `examples/`, not a crate.** `examples/litellm` is the precedent. A
   `examples/gateway-api-inference/` with a minimal host that drives `run_stream` and maps
   logical models to Gateway API Inference Extension routes would show any GAIE gateway how
   to sit behind Switchyard. Vendor-neutral: the same shape works for BNK, kgateway or
   Envoy AI Gateway. The F5-specific manifests stay in this repository.
3. **Document the trust boundary of `Metadata::from_headers`.** It reads client
   `x-switchyard-*` headers, which is right for routing hints and wrong for identity. A
   sentence in the docs saves the next host author from using it for tenancy.
4. **Usage on streamed routing calls.** Hosts that want accounting must inject
   `stream_options.include_usage` themselves before decoding, because same-format replay
   keeps the original body. A translation option for it would be more direct. Lower priority;
   it is a few lines in a host.

Nothing here needs a change to libsy's routing algorithms.

## What F5 could add

1. **An AIModel deployment that targets an InferencePool.** In 2.4.2 an `AIProvider` is a URL and
   pointing it at a BNK Gateway VIP is reset, so traffic governed by the AI gateway cannot reach the
   F5 EPP. A `backendRef` to an InferencePool on the deployment (as HTTPRoute has) would join the two
   paths: Polaris for identity and budgets, its router slot for Switchyard, the EPP for the pod.
2. A documented statement on header-match HTTPRoutes with several InferencePool backends.
3. Whether the EPP logs or exports a request ID, so per-request EPP decisions can be joined
   to adapter traces.
4. A per-request "chosen endpoint" response header. GAIE conformance says the Gateway reports
   which endpoint served a request; documenting the header name would let the adapter pass
   it to clients instead of relying on the model server's own header.
