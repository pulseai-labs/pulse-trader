# r6 registered runs under freeze 30f98b21

- **Registered:** before any of the eight runs ran. This file's first commit on canonical is the registration.
- **Decided by:** operator (PDR-0044 decision 3; PDR-0047). Strategy: STR-0003 (v1 and v2, registered 2026-10-20).
- **Rule:** this file is never edited after the first run starts. A change to a file, a pair or a constant below is a
  new registration and a new hypothesis.

## Runs

Each run is judged on its own. Each is one `pulse certify power --version <id> --pair <PAIR>` on prod, against a
strategy version whose stored DSL equals the file below byte for byte.

| Run | File | SHA-256 | Pair |
|---|---|---|---|
| R1 | examples/strategies/cipher-v1.json | dc10ca4069eca64609b263ce8631a8a15a0c44349bda025577746d57cd5d0e32 | BTCUSDT |
| R2 | examples/strategies/cipher-v1.json | dc10ca4069eca64609b263ce8631a8a15a0c44349bda025577746d57cd5d0e32 | ETHUSDT |
| R3 | examples/strategies/cipher-v1.json | dc10ca4069eca64609b263ce8631a8a15a0c44349bda025577746d57cd5d0e32 | SOLUSDT |
| R4 | examples/strategies/cipher-v1.json | dc10ca4069eca64609b263ce8631a8a15a0c44349bda025577746d57cd5d0e32 | XRPUSDT |
| R5 | examples/strategies/cipher-v2.json | 63706b6ee917448cb11e6f0da2185e248aac1edc366142fdb4a714667eeb703c | BTCUSDT |
| R6 | examples/strategies/cipher-v2.json | 63706b6ee917448cb11e6f0da2185e248aac1edc366142fdb4a714667eeb703c | ETHUSDT |
| R7 | examples/strategies/cipher-v2.json | 63706b6ee917448cb11e6f0da2185e248aac1edc366142fdb4a714667eeb703c | SOLUSDT |
| R8 | examples/strategies/cipher-v2.json | 63706b6ee917448cb11e6f0da2185e248aac1edc366142fdb4a714667eeb703c | XRPUSDT |

## Constants

| Name | Value |
|---|---|
| freeze_id | 30f98b21-cb1d-4103-9393-a71be01d9dbf |
| holdout_start | 2026-10-10T00:00:00Z |
| h | 2 |
| rule | wf-v2 |
| k | 6 |
| folds_required | 4 |
| n_min | 20 |
| wf_v2_z | 1.645 |
| power_bar | 0.5 |
| assumed_edge_r | 0.25 |
| min_holdout_trades | 20 |
| primary_timeframe | 15m |
| htf_timeframe | 4h |

## How each run is judged

- **Search span:** from the first fully-warm bar to the holdout start, on the latest candles prod holds for the pair
  when the run starts. The span is never set by hand.
- **wf-v2:** k folds; a fold holds on n ≥ n_min and a mean R above 0; the run passes on folds_required holding folds
  and a pooled lower bound (wf_v2_z) above 0. R is net of fees and funding (ADR-0028 amendment, r6.s3).
- **Power review:** pass chance at the holdout's current trade count, at μ = assumed_edge_r and the run's own
  search-span σ, at this freeze's h. An attempt is allowed only at n ≥ min_holdout_trades and a pass chance ≥ power_bar
  (ADR-0032).
- **Certify:** only a run whose power review allows it. At most h attempts under this freeze. If more than h runs are
  allowed, the operator picks h of them before any certify call.
- **Outcome:** `docs/certification/r6-runs-<date>.md`, written when the eight runs have run (r6.s5.w3).
