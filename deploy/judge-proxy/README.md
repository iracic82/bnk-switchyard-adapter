# Judge proxy

Lets lab sandboxes ask NVIDIA's hosted API for Switchyard's judge verdicts without holding the
NVIDIA key. An AWS Lambda behind a function URL; the NVIDIA key and the token-signing key live in
AWS Secrets Manager, per-lab call counts in DynamoDB.

- `handler.py` checks a lab token (HMAC-signed `{sid, exp, q}`), caps it by its own limits
  (`MAX_CALLS`, `MAX_HOURS`, a `DAILY_CAP` for all labs together), allows only the judge model on
  `POST /v1/chat/completions`, rebuilds the body from an allowlist (`n=1`, output capped), merges
  `EXTRA_BODY` and forwards. Prompts are not logged.
- `mint_token.py` mints a token from `JUDGE_PROXY_SIGNING_KEY` in the environment.

The lab's setup mints a token for its own sandbox with the Instruqt secret
`JUDGE_PROXY_SIGNING_KEY` and forgets the key (`scripts/instruqt-judge.sh`).

`EXTRA_BODY` is `{"chat_template_kwargs": {"enable_thinking": false}}`: Nemotron 3.5 Lightning is a
reasoning model, and with reasoning on it spends its output budget thinking and returns no verdict.
With it off, a verdict takes 1 to 2 s.

Resources (us-east-1, tag `project=switchyard-lab`): function and role `switchyard-judge-proxy`,
secrets `switchyard-judge/signing-key` and `switchyard-judge/nvidia-api-key`, table
`switchyard-judge-quota` (TTL on `ttl`), bucket `switchyard-judge-proxy-<account>` for the code.
To rotate the signing key, change it in Secrets Manager and in the Instruqt secret together, then
update the function's configuration so it reloads its secrets.
