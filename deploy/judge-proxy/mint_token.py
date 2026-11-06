#!/usr/bin/env python3
"""Mints a lab token for the judge proxy (deploy/judge-proxy/handler.py).

    JUDGE_PROXY_SIGNING_KEY=... python3 mint_token.py <sandbox id> [hours=8] [calls=1500]

The signing key is read from the environment, never from the command line.
"""
import base64
import hashlib
import hmac
import json
import os
import sys
import time


def mint(key, sid, hours=8, calls=1500):
    payload = base64.urlsafe_b64encode(json.dumps(
        {"sid": sid, "exp": int(time.time() + hours * 3600), "q": calls}, separators=(",", ":")).encode()).rstrip(b"=")
    sig = base64.urlsafe_b64encode(hmac.new(key.encode(), payload, hashlib.sha256).digest()).rstrip(b"=")
    return (payload + b"." + sig).decode()


if __name__ == "__main__":
    key = os.environ.get("JUDGE_PROXY_SIGNING_KEY", "")
    if not key or len(sys.argv) < 2:
        sys.exit(__doc__)
    print(mint(key, sys.argv[1], *(float(a) for a in sys.argv[2:3]), *(int(a) for a in sys.argv[3:4])))
