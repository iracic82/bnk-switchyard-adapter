#!/usr/bin/env python3
"""Synthetic agent workload: episodes of 3-6 steps that mix easy and hard work.

Each step's user message carries a tag `[task:<id>:easy|hard]`. mock-llm uses the tag to
simulate success (see crates/mock-llm); for servers that do not, `step_outcome` simulates it
with the same rates. The mock judge ignores the tag and judges
the wording, so routing is imperfect on purpose:

- most hard steps use wording a judge flags as hard ("design", "debug", "prove" ...)
- some hard steps use plain wording (the judge under-routes them)
- some easy steps contain hard-sounding words (the judge over-routes them)

An episode succeeds only if every step succeeds, as with a real agent task.
"""

import argparse
import json
import random

EASY = [
    "Summarize this support ticket in one sentence: customer cannot log in after a password reset.",
    "Extract the order number from: 'Hi, my order 88231 arrived damaged.'",
    "Translate 'good morning' into Spanish.",
    "Format these three fruit names as a JSON list: apple, pear, plum.",
    "Which HTTP status code means not found?",
    "Rewrite this sentence in a friendlier tone: 'Your request is denied.'",
    "List the files that the previous tool call returned, one per line.",
    "Convert 72 degrees Fahrenheit to Celsius.",
    "Classify this message as billing, technical or other: 'I was charged twice.'",
    "Write a one-line commit message for a typo fix in the README.",
]
EASY_TRICKY = [  # easy, but the wording trips a keyword judge
    "What does the word 'design' mean? One sentence.",
    "Spell 'debug' backwards.",
]
HARD = [
    "Design a rate limiter for 50,000 requests per second across three regions and explain the trade-offs.",
    "Debug this race condition: two workers update the same account balance and the total drifts.",
    "Refactor the payment module to remove the circular dependency with orders, and prove behaviour is unchanged.",
    "Derive the time complexity of this recursive scheduler and propose an optimization.",
    "Find the root cause of this distributed deadlock between the queue consumer and the lock service.",
    "Migrate this schema from a single table to an event log without downtime; give the multi-step plan.",
    "Architect a retry and idempotency strategy for a payment API with at-least-once delivery.",
]
HARD_PLAIN = [  # hard, with no wording a keyword judge notices
    "Plan how we move the billing system to a new database while customers keep paying.",
    "Work out why the nightly report totals differ from the ledger by a few cents each month.",
]

STEP_KINDS = [
    # (kind, probability the step is hard)
    ("plan", 0.6),
    ("read", 0.1),
    ("tool", 0.0),
    ("fix", 0.7),
    ("answer", 0.1),
]


# mock-llm's default success rates (crates/mock-llm): (easy, hard) per model size.
SUCCESS = {False: (0.95, 0.3), True: (0.98, 0.9)}


def step_outcome(text: str, step: dict, large: bool, salt: str = ""):
    """Whether a step succeeded, and whether that was simulated here.

    mock-llm writes `RESULT: PASS|FAIL` into its answer. Other servers (llm-d's simulator, real
    vLLM) do not, so the outcome is drawn with mock-llm's rates for the model that served the
    step. The draw is seeded by step and salt, so reruns of the same run agree.
    """
    if "RESULT:" in text:
        return "RESULT: PASS" in text, False
    p = SUCCESS[large][1 if step["hard"] else 0]
    return random.Random(f"{step['id']}/{salt}").random() < p, True


def generate(episodes: int, seed: int, hard_share: float):
    rng = random.Random(seed)
    out = []
    for e in range(episodes):
        steps = []
        for i in range(rng.randint(3, 6)):
            kind, p_hard = rng.choice(STEP_KINDS)
            hard = rng.random() < min(1.0, p_hard * hard_share / 0.3)
            if hard:
                text = rng.choice(HARD_PLAIN) if rng.random() < 0.15 else rng.choice(HARD)
            else:
                text = rng.choice(EASY_TRICKY) if rng.random() < 0.05 else rng.choice(EASY)
            steps.append({"id": f"e{e}-s{i}", "hard": hard, "kind": kind,
                          "prompt": f"[task:e{e}-s{i}:{'hard' if hard else 'easy'}] {text}"})
        out.append({"episode": f"e{e}", "steps": steps})
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--episodes", type=int, default=60)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--hard-share", type=float, default=0.3,
                        help="scales the chance that a step is hard; 0.3 is the default mix")
    args = parser.parse_args()
    for episode in generate(args.episodes, args.seed, args.hard_share):
        print(json.dumps(episode))


if __name__ == "__main__":
    main()
