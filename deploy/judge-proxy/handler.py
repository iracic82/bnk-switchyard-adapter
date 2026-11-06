"""Judge proxy: lets lab sandboxes call NVIDIA's hosted API without holding the NVIDIA key.

A lab's setup mints a token for its own sandbox, signed with a key only setup sees:
    base64url(payload) "." base64url(HMAC-SHA256(signing_key, base64url(payload)))
    payload = {"sid": sandbox id, "exp": unix seconds, "q": max calls}
The proxy checks the signature and expiry, counts calls per sandbox in DynamoDB, allows only
POST /v1/chat/completions for the configured models (non-streaming, output capped), and forwards
with the NVIDIA key from Secrets Manager. Prompts and answers are never logged.

The proxy does not trust the token's limits beyond its own: a token may ask for at most MAX_CALLS
calls and expire at most MAX_HOURS ahead, and all labs together get at most DAILY_CAP calls a day,
so even a leaked signing key cannot spend more than that. The upstream body is rebuilt from an
allowlist of fields, with n=1 and the output capped.

Environment: UPSTREAM, MODELS (comma list), MAX_TOKENS_CAP, TABLE, NVIDIA_SECRET, SIGNING_SECRET,
MAX_CALLS (1500), MAX_HOURS (9), DAILY_CAP (20000), MAX_BODY_BYTES (65536).
"""

import base64
import hashlib
import hmac
import json
import os
import time
import urllib.error
import urllib.request

import boto3

_secrets = boto3.client("secretsmanager")
_table = boto3.resource("dynamodb").Table(os.environ["TABLE"])
_cache = {}
ALLOWED = ("model", "messages", "max_tokens", "temperature", "top_p", "seed", "stop", "response_format")


def _secret(name):
    if name not in _cache:
        _cache[name] = _secrets.get_secret_value(SecretId=os.environ[name])["SecretString"].strip()
    return _cache[name]


def _reply(status, body):
    return {"statusCode": status, "headers": {"content-type": "application/json"},
            "body": body if isinstance(body, str) else json.dumps(body)}


def _err(status, code, message):
    return _reply(status, {"error": {"code": code, "message": message, "type": "judge_proxy"}})


def _b64d(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def verify(token, key, now=None):
    """Returns the token's payload, or None when it is malformed, forged or expired."""
    try:
        p, sig = token.split(".", 1)
        want = base64.urlsafe_b64encode(hmac.new(key.encode(), p.encode(), hashlib.sha256).digest()).rstrip(b"=")
        if not hmac.compare_digest(want, sig.encode()):
            return None
        claims = json.loads(_b64d(p))
        now = now or time.time()
        if not isinstance(claims.get("sid"), str) or not 0 < len(claims["sid"]) <= 128:
            return None
        if not now < claims.get("exp", 0) <= now + float(os.environ.get("MAX_HOURS", "9")) * 3600:
            return None
        if not 0 < int(claims.get("q", 0)) <= int(os.environ.get("MAX_CALLS", "1500")):
            return None
        return claims
    except Exception:
        return None


def handler(event, _context):
    http = event.get("requestContext", {}).get("http", {})
    if http.get("method") != "POST" or event.get("rawPath") != "/v1/chat/completions":
        return _err(404, "not_found", "only POST /v1/chat/completions")
    auth = (event.get("headers") or {}).get("authorization", "")
    claims = verify(auth[7:] if auth.lower().startswith("bearer ") else "", _secret("SIGNING_SECRET"))
    if not claims:
        return _err(401, "bad_token", "missing, forged or expired lab token")
    raw = event.get("body") or ""
    if event.get("isBase64Encoded"):
        raw = base64.b64decode(raw).decode()
    if len(raw) > int(os.environ.get("MAX_BODY_BYTES", "65536")):
        return _err(413, "too_large", "the request is larger than a judge call needs")
    try:
        sent = json.loads(raw)
    except ValueError:
        return _err(400, "bad_json", "the body is not JSON")
    if not isinstance(sent, dict) or not isinstance(sent.get("messages"), list):
        return _err(400, "bad_request", "a chat completion needs messages")
    if sent.get("model") not in os.environ["MODELS"].split(","):
        return _err(403, "model_not_allowed", "this proxy serves the lab's judge model only")
    if sent.get("stream"):
        return _err(400, "no_streaming", "streaming is not proxied")
    body = {k: sent[k] for k in ALLOWED if k in sent}
    cap = int(os.environ.get("MAX_TOKENS_CAP", "512"))
    asked = sent.get("max_tokens") or sent.get("max_completion_tokens") or cap
    body["max_tokens"] = max(1, min(int(asked), cap))
    body["n"] = 1
    ttl = int(claims["exp"]) + 86400
    for key, limit, msg in [
        (claims["sid"], int(claims["q"]), "this lab has used its judge calls"),
        ("all-labs#" + time.strftime("%Y-%m-%d", time.gmtime()), int(os.environ.get("DAILY_CAP", "20000")),
         "the judge proxy has reached today's limit for all labs")]:
        try:
            _table.update_item(
                Key={"sid": key},
                UpdateExpression="ADD n :one SET #t = :ttl",
                ConditionExpression="attribute_not_exists(n) OR n < :q",
                ExpressionAttributeNames={"#t": "ttl"},
                ExpressionAttributeValues={":one": 1, ":q": limit, ":ttl": ttl})
        except _table.meta.client.exceptions.ConditionalCheckFailedException:
            return _err(429, "quota", msg)
    req = urllib.request.Request(os.environ["UPSTREAM"].rstrip("/") + "/chat/completions",
                                 data=json.dumps(body).encode(), method="POST",
                                 headers={"Authorization": "Bearer " + _secret("NVIDIA_SECRET"),
                                          "Content-Type": "application/json", "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=25) as r:
            return _reply(r.status, r.read().decode())
    except urllib.error.HTTPError as e:
        print(json.dumps({"sid": claims["sid"], "upstream_status": e.code}))
        return _reply(e.code, e.read().decode() or json.dumps({"error": {"code": "upstream"}}))
    except Exception as e:
        print(json.dumps({"sid": claims["sid"], "upstream_error": type(e).__name__}))
        return _err(502, "upstream_unreachable", "NVIDIA's API did not answer")
