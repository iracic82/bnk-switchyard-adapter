# Troubleshooting

Start with the response headers. Every routed response carries `x-request-id`,
`x-switchyard-route`, `x-switchyard-algorithm`, `x-switchyard-model`,
`x-switchyard-provider-model`, `x-switchyard-inference-pool` and
`x-switchyard-decision-ms`. Errors are OpenAI-style JSON with a stable `error.code`. Then
search the adapter's JSON logs for the request ID:

```bash
kubectl -n switchyard logs deploy/switchyard-adapter | grep '<request-id>'
```

## Errors from the adapter

| Status / code | Meaning | What to check |
|---|---|---|
| 401 `unauthenticated` | No bearer key, or its SHA-256 is in no tenant's `api_key_sha256` | `printf '%s' "$KEY" \| sha256sum` and compare with the ConfigMap |
| 403 `model_not_allowed` | The client named a model outside the tenant's `allowed_models` | Intended. Change the tenant config if it shouldn't be |
| 403 `route_not_allowed` | The route is not in the tenant's `allowed_routes` | `allowed_routes = []` allows direct model names only |
| 403 `tier_not_allowed` | The route needs a tier (for example `capable`) the tenant may not use | Add the model, or set `tier_fallback = "degrade"` |
| 400 `max_tokens` | `max_tokens` or `max_completion_tokens` above the tenant's cap | |
| 429 `rate_limited`, `concurrency_limited`, `budget_exhausted` | Tenant limits. Limits are per adapter replica | `sy_adapter_rejections_total{reason}` |
| 404 `model_not_found` | `model` names neither a route nor a logical model | |
| 413 | Body larger than `server.max_body_bytes` | |
| 508 `loop_detected` | The request already carried `x-switchyard-adapter-hop` | A route sends adapter traffic back to the adapter; see below |
| 504 `routing_timeout` | The algorithm did not finish within `routing_timeout_secs` | Judge pool health; `sy_adapter_routing_calls_total{outcome="error"}` |
| 502 / 503 / 504 with the inference server's error body | The answer call failed and no fallback applied | The pool: Gateway, HTTPRoute, InferencePool, EPP, pods |
| 500 `policy_violation` | An algorithm chose a model outside the plan. This is a bug | Open an issue with the request ID |

## A 508 loop

The adapter refuses requests that already passed through it. If you see 508 in steady
traffic, one of the adapter's model `base_url`s resolves to the external Gateway, or an
HTTPRoute on an internal Gateway points back at the adapter Service. Check:

```bash
kubectl -n switchyard get cm -o yaml | grep base_url
kubectl get httproute -A -o custom-columns=NS:.metadata.namespace,NAME:.metadata.name,PARENT:.spec.parentRefs[*].name,BACKEND:.spec.rules[*].backendRefs[*].name
```

Every `base_url` must be an internal Gateway VIP. The external Gateway's only route must be
`switchyard/switchyard-external`.

## The request reaches the wrong model, or 404 "model does not exist"

vLLM (and the mock) refuse an unknown model name with 404. That means the request reached a
pool that does not serve that `provider_model`. Check that `provider_model` in the adapter
config equals the served model name (`--model` for vLLM, `MOCK_MODEL` for the mock), and that
the model's `base_url` is the VIP of the Gateway whose HTTPRoute points at that model's pool.

## Gateway or route problems

```bash
kubectl get gateway -n f5-bnk                    # PROGRAMMED must be True
kubectl -n inference describe httproute small-pool
```

`ResolvedRefs=False` on an HTTPRoute that names an InferencePool means the pool doesn't
exist or is in another namespace. The route and the pool must share a namespace. An
`Accepted=False` route usually means the Gateway's `allowedRoutes` selector excludes the
route's namespace; these manifests allow `switchyard` on the external Gateway and
`inference` on the internal ones only.

## Gateway Programmed=False with AddressNotUsable

BNK only accepts a static Gateway address inside the range of the `F5BnkGateway`'s
`ingressConfig.defaultListenerNetworks` (and its IPAM range). Anything else is accepted but never
programmed: `Gateway has an address that cannot be used by this implementation`, and the
controller logs `has unusable static address`. Check the range and pick a free address in it:

```bash
kubectl get f5-bnkgateways.k8s.f5net.com -A -o jsonpath='{.items[0].spec.ingressConfig.defaultListenerNetworks}'
kubectl get gateway -A -o jsonpath='{range .items[*]}{.spec.addresses[0].value}{"\n"}{end}' | sort
```

## EPP logs `metric family "vllm:cache_config_info" not found`

The F5 EPP's metrics extractor reads vLLM's `vllm:cache_config_info`. Real vLLM exports it;
mock-llm does from 0.2.1 on (simulated values). For another mock or engine without it, either add
the series or blank `cacheInfoSpec` in the F5EPP's `core-metrics-extractor` parameters.

## F5EPP not Available

```bash
kubectl -n inference get f5epp -o wide
kubectl -n inference describe f5epp small-pool-epp
kubectl -n inference get pods | grep epp
```

The controller renders nothing unless `inference` is listed in
`CNEInstance.spec.eppNamespaces`. An EPP pod in `ImagePullBackOff` with 403 means the F5
registry pull secret is missing from `inference`. An EPP stuck `Pending` with a tokenizer
cache claim means the PVC can't bind.

`spec.tokenizer.name` must equal the model name the pods serve. A mismatch does not fail
the request: the EPP can't tokenize, prefix-cache scoring stops, and it falls back to a
cold picking path. On a BNK 2.4 lab this showed up as `epp_tokenizer_guard_events_total{event="encode_failed"}`
counting every request.

With the mock model servers, the `zmq-wire` plugin is removed because the mocks publish no
KV-cache events. With real vLLM, keep it and publish events (see `models-real/vllm.yaml`).

## The EPP seems to pick badly with mock servers

The mocks export `vllm:num_requests_running`, `vllm:num_requests_waiting` and
`vllm:kv_cache_usage_perc`, but the KV gauge is slots in use, not memory. Load spreading
by queue depth works; prefix-cache affinity can't be shown with mocks. Use real vLLM for
that.

## Classifier routes everything to the large model

The task classifier fails open to its capable tier when the judge call fails or times out.
Look at the routing evidence in the adapter log (`"routed"` events):
`{"source":"llm-classifier","score":...}` means the judge answered; anything else is a
fallback. Check `sy_adapter_routing_calls_total{kind="model",outcome="error"}` and the
small pool's health, since the judge runs on it. `judge_deadline_ms` bounds one judge call.

## Streams without tokens or cost

The adapter asks upstream for `stream_options.include_usage`. A backend that ignores the
option reports no usage: the request is logged with `usage_reported=false` and counts zero
tokens and zero cost. vLLM honours the option.

## Requests fail for a few seconds after a rollout

The adapter drains on SIGTERM: `/readyz` turns 503 and it keeps serving for
`shutdown_drain_secs` (5 s) before it closes. If clients still see connection errors after
rollouts, raise `shutdown_drain_secs` and keep `terminationGracePeriodSeconds` above it plus
your longest stream.

## Local stack

`scripts/local-up.sh` logs to `.run/*.log`. "Address already in use" means another process
holds 9480 to 9482, often an adapter still draining; `scripts/local-down.sh` waits for it.
