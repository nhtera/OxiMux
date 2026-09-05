# Automatic retry after a rate limit

When a turn fails because the provider is out of capacity, OxiMux can hold the
turn and send it again by itself, instead of leaving a failed turn for you to
find and re-send by hand.

It is deliberately conservative: it retries only when a machine-readable signal
says waiting will actually help.

## What gets retried

| The provider said | Class | What happens |
|---|---|---|
| A usage window is full (`five_hour`, `seven_day`, `seven_day_opus`, `seven_day_sonnet`) and named its reset | **Window** | Re-sent shortly after the reset |
| HTTP 429 / 503 / 529 with no window reading | **Overload** | Re-sent after 30s, doubling per attempt |
| The account is billing past its plan (`overage`, `seven_day_overage_included`) | **Spend** | **Never** re-sent |
| A window kind this build does not recognise | Other | Never re-sent |
| A window is full but the provider withheld the reset | Other | Never re-sent |
| Any other error (bad request, auth, a crash) | Other | Never re-sent |

Two of those rows are the ones worth knowing about.

**Overage is not a closed window.** When an account passes its plan allowance,
requests keep working — they just start costing money. Retrying that
automatically would spend on your behalf without asking, so it never happens.
The turn fails and you decide.

**An unknown limit kind is treated as not retryable.** Providers add window
kinds. A build that has never heard of one surfaces the error rather than
guessing that waiting will fix it. Under-retrying costs a click; over-retrying
can cost money or hammer an account that is already refusing requests.

A turn you stopped yourself is never retried, and neither is one whose agent
process has died.

## Jitter, and why it matters

Every thread on one account sees the *same* reset time. Without a random spread
they would all wake in the same second and re-limit the account instantly — the
retry would cause the outage it exists to survive. So every wake time gets a
random offset of up to two minutes. This is load-bearing, not a refinement.

## Limits

- **Four attempts** per turn, maximum. Not configurable.
- Attempts reset when you send something new, so a long conversation that hits a
  limit once an hour is never locked out.
- Cancelling a queued retry does not count as an attempt.

## The queued card

While a retry is armed the thread shows a card in place of the error:

```
Attempt 1 of 4
Usage limit reached — retrying in 3h 10m
[ Send now ]  [ Cancel ]
```

- **Send now** fires immediately and counts as an attempt.
- **Cancel** drops the retry and shows the original error.

The countdown updates itself — every 30 seconds while the wait is long, every
second in the last minute.

## Setting: maximum automatic wait

A weekly window can reset days out. Waiting that long silently is worse than
reporting the error: a queued turn and a forgotten one look identical, and a
turn that fires two days late lands in a context you have moved on from.

So the wait is capped. A reset farther out than the cap is not scheduled at all;
the failure surfaces as an ordinary error.

| Setting | Effect |
|---|---|
| Up to 6 hours | Covers a five-hour window, not a weekly one |
| **Up to 24 hours** (default) | Covers every five-hour window and a same-day weekly reset |
| No limit | Waits however long the provider says |

Stored in `agent_retry.toml` in the app data directory:

```toml
enabled = true
max_automatic_wait = "one-day"   # or "six-hours", "no-limit"
```

Setting `enabled = false` restores the previous behaviour — a failed turn is
reported immediately and never held.

## Where the vocabulary comes from

The limit kinds above are not guessed from error messages, and not matched
against error prose — a provider is free to reword a message at any time. They
are read from the `rate_limit_event` line that the Claude CLI already emits on
its own stream, whose `status` and `rateLimitType` values are a closed set
defined by the CLI's own schema. OxiMux previously discarded that line.

One detail worth recording: the wire reports `resetsAt` in unix **seconds**,
while every reset time inside OxiMux is milliseconds. The conversion happens
once, in the decoder, and is asserted against a literal — get it wrong and a
retry fires either instantly or tens of thousands of years out, neither of which
looks like a units bug from the UI.

## Not covered yet

`oximux agent retry status|now|cancel` is not implemented. Retry state currently
lives in the desktop host only; exposing it to the CLI needs a new relay verb
and a protocol version bump.
