# DOMAINS — evolvable knob domains for every Lua strategy

> `feat/evolution-domains` — declares `min`/`max` for every numeric tunable of all 10
> `user_layer/strategies_lua` packages, so each opts into shadow evolution (the #393
> loader rule: a tunable with a declared domain becomes a `KnobSpec`; a package with
> none stays *not evolvable*). Every domain (a) covers the declared default, (b) has
> an economically real floor AND ceiling — no hedge-eating wilderness, no off-switches
> disguised as bounds — and (c) touches no stop/止损-family knob (charter §16.4: the
> kernel owns survival; `scripts/strategy-no-stop-loss-check.mjs` stays green).

Wire rule (§6.4): tunable values are decimal **strings**. `specs_from_tunables()`
parses `min`/`max`/`default` with `Decimal::from_str_exact` and requires the domain
coherent (`min <= default <= max`) — an incoherent or unparseable domain is skipped,
so these tables are the contract, not prose.

The machinery that consumes them (`shadow_evolution/`): one unit per strategy, a
shadow twin set under the guard's domain lock, E13 proposals (default OFF for
auto-apply, 72h DEEP rounds). How to run it: `--shadow-evolution` + the `--se-*`
tuning flags (see "How to turn evolution on", bottom).

Coverage: **69 knobs across 10 strategies** — every numeric
tunable declared. The two `string`-typed tunables (`spot_missing` in mad_dog and
market_maker) are policies, not magnitudes; `specs_from_tunables()` only accepts
`decimal`/`int`, so they stay plain `bk.params()` entries — correct by construction.

## pair_discount_arb

Pair-discount collector: both legs at their asks + fees < max_pair_cost, merged into $1.00/pair. Profit locked at entry; exit = collection, never a decision.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `max_pair_cost` | decimal | `0.995` | [0.98, 0.999] | cap on the all-in pair cost (both asks + both entry fees) vs the $1.00 merge redemption: the upper edge 0.999 is the last cap under which a filled pair still cannot lose (total < 1); the lower edge 0.98 demands a >2% locked edge, the strictest regime that still fills on ordinary books — beyond it the collector starves. |
| `min_leg_price` | decimal | `0.01` | [0.01, 0.10] | floor on each leg's ask: 0.01 is the venue's 0.01 price grid absolute floor (a leg cannot be quoted below it); at 0.10 both legs must sit >= 0.10, which abandons the cheap-leg discount mechanism for a mid-band spread bet — the identity boundary. |
| `max_leg_price` | decimal | `0.99` | [0.50, 0.995] | ceiling on each leg's ask: below 0.50 BOTH legs are cheap and the gate stops discriminating books with an expensive side (the discount thesis dissolves into 'any two-sided cheap book'); above 0.995 no mergeable pair can exist (0.995 leaves no room for the other leg plus fees), so the ceiling is inert past it. |
| `min_pair_shares` | decimal | `1` | [1, 100] | floor on the declared share count (both legs equal, floored to the 0.01 grid): 1 = one share-pair, the grid's minimum honest unit; 100 forces >= ~$100 notional per leg, above which the min-share floor overrides depth-following sizing on most books and turns the sizer fixed. |
| `min_time_left_sec` | int | `30` | [0, 300] | entry time floor: 0 defers entirely to the kernel's own timing gate; 300 = only the last five minutes of a 900s round, which turns the always-on collector into a late-window sniper (a different strategy). Collection ('merge') is NOT bounded by this knob — it runs to the last tick by design. |

## single_leg_pair

Single-leg round participant (#387, no-stop revision #394): buys the sole liftable leg or the in-band leader, holds naked to settlement. Kernel owns every exit.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `min_entry_price` | decimal | `0.55` | [0.50, 0.80] | entry-band floor, priced from the strategy's own ledger odds table (header): below 0.50 lies the coin-flip/lottery zone the package refuses by design; 0.80 keeps a possible band >= 0.05 wide against the max_entry_price domain. |
| `max_entry_price` | decimal | `0.85` | [0.60, 0.95] | entry-band ceiling: 0.95 is the last band with a ledger-listed positive EV (0.80-0.90 +0.120/share, >=0.90 +0.065); beyond 0.95 residual upside can no longer pay fees on a naked directional leg. |
| `dead_band_floor` | decimal | `0.50` | [0.40, 0.55] | lower edge of the refused coin-flip zone: below 0.40 the 'dead band' starts legitimizing the lottery zone (0.10-0.30 is EV -0.05..-0.13 per the ledger); above 0.55 it eats into the favourable band's bottom. |
| `min_gap` | decimal | `0.02` | [0.01, 0.15] | leader-mode minimum ask gap: 0.01 = one venue tick, the smallest gap that deserves to be called a directional verdict; beyond 0.15 the leader branch goes silent on nearly all books and the package degrades to sole-leg-only. |
| `min_time_left_sec` | int | `45` | [0, 300] | strategy's own time floor ON TOP of the kernel timing gate (which is NOT waived here — a directional bet): 0 defers to the kernel; 300 = late-window sniper boundary, same reasoning as pair_discount_arb. |
| `max_open_positions` | int | `3` | [1, 10] | per-round naked-exposure cap: 1 = strictest (one naked leg at a time); beyond 10 the kernel's own notional/positions caps dominate and the knob stops being a risk lever the strategy meaningfully owns. |
| `min_shares` | decimal | `1` | [1, 100] | floor on the declared share count: same dust-vs-fixed-size reasoning as pair_discount_arb's min_pair_shares. |

No stop/止损 knob exists here and none may be added: #394 revised this package to
express no exits at all (charter §16.4, gate-enforced). `max_open_positions` is an
exposure CAP — it shrinks risk; it is not a stop.

## spread_arb

Trend-confirmed dip buyer (exact Lua port of the Rust reference): confirmed trend rests a bid at mid*factor under the entry ceiling.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `trend_min_price` | decimal | `0.55` | [0.50, 0.80] | mid level a side must hold for the trend ratio: 0.50 = a side below even odds is not a confirmed trend; 0.80 makes confirmation alongside a 0.35 break nearly unsatisfiable (same family as mad_dog's dominant_min_price). |
| `trend_confirm_sec` | int | `60` | [10, 300] | confirmation window: 10 = the code's own floor (confirm_ms = max(confirm_sec*1000, 10000) — below it the domain merely restates the clamp); 300 = a third of the round. |
| `trend_broken_price` | decimal | `0.35` | [0.10, 0.50] | regime-break floor (mid below it cancels the trend and the resting bids): 0.10 keeps the break regime alive (the code disables the check at 0 — the domain refuses to let evolution switch the regime off); 0.50 = the dominance overlap boundary. |
| `trend_entry_price` | decimal | `0` | [0, 0.90] | fixed entry price (0 = OFF, the mid*factor formula runs instead): 0 stays reachable (the default and pre-gate behaviour); 0.90 = the code's own hard entry ceiling (DEC_090) — a fixed price above it is inert. |
| `trend_entry_factor` | decimal | `0.88` | [0.50, 0.99] | entry = mid * factor: 0.50 = half-of-mid insult bids that fill only in halvings; 0.99 = the thinnest passive discount that still survives the entry < mid guard after round2. |
| `trend_max_entry_price` | decimal | `0.45` | [0.05, 0.90] | ceiling on the computed entry: 0.05 = the code's own entry floor (DEC_005 — below it every entry is blocked, i.e. off); 0.90 = the code's entry ceiling (DEC_090). |
| `entry_min_obi` | decimal | `0` | [0, 0.9] | minimum bid-side order-book imbalance (0 = OFF, the default): 0.9 = only maximally bid-heavy books qualify — the strongest demand that still occurs. |
| `entry_max_spread_pct` | decimal | `0` | [0, 10] | max tolerated spread % (0 = OFF, the default): 10 = permissive on 15m binary books (spreads routinely run 5-12% of mid); beyond it the gate never vetoes. |
| `entry_dip_max_pct` | decimal | `0` | [0, 50] | max dip below the recent high (0 = OFF, the default): 50 = a halving tolerated; beyond it the gate is off by looseness. |
| `entry_bounce_min_pct` | decimal | `0` | [0, 10] | minimum bounce % required over the bounce window (0 = OFF, the default): 10 inside a 5s window is flash-scale — the near-unreachable extreme. |
| `entry_bounce_window_sec` | int | `5` | [1, 60] | bounce measurement window: 1 = the fastest real span; 60 = a minute-long 'bounce' is trend territory, not a bounce. |
| `entry_trend_window_sec` | int | `0` | [0, 600] | slide-gate memory (0 = OFF, the default and pre-gate behaviour): 600 = two thirds of a 900s round — beyond it the gate reads the round's whole shape, not a slide. |
| `entry_trend_drop_pct` | decimal | `30` | [1, 99] | slide veto threshold (drop <= -this % over the window blocks the entry): 1 = hair-trigger; 99 = only a near-total collapse vetoes (nominally on, effectively off). The code's 0-would-refuse-everything guard is why the floor sits at 1, not 0. |

## market_maker

Passive spread harvester: cheap, deep, balanced, calm chip earns a maker bid bid_offset under mid; take_profit_spread banks the spread.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `min_mid_price` | decimal | `0.20` | [0.05, 0.40] | chip-band floor: 0.05 = the ledger's lottery boundary (MM'ing sub-0.10 chips is adverse-selection city); 0.40 keeps a possible band against max_mid_price's domain. |
| `max_mid_price` | decimal | `0.42` | [0.25, 0.50] | chip-band ceiling: 0.25 keeps a band >= [0.20,0.25] against min_mid_price's default; 0.50 = the coin-flip boundary — above it the package would be inventorying the market's FAVOURED side, abandoning the cheap-chip vol thesis. |
| `bid_offset` | decimal | `0.02` | [0, 0.15] | resting-bid distance under mid: 0 = bid at mid (still non-crossing — the entry < ask guard holds, one tick of spread suffices); 0.15 under a <=0.50 chip is a <=0.35 insult bid that fills only in crashes — mad_dog's job, not the harvester's. |
| `take_profit_spread` | decimal | `0.03` | [0.01, 0.30] | banked spread (mid >= bid + this -> close suggestion): 0.01 = one tick, the thinnest bankable spread on the venue grid; 0.30 on a <=0.50 chip demands the mid to nearly double within the round — the target becomes unreachable and the take-profit silently off. |
| `min_time_left_sec` | int | `120` | [0, 300] | entry time floor (the close-suggestion path deliberately bypasses it): 0 defers to the kernel gate; 300 = sniper boundary. |
| `min_bid_depth` | decimal | `300` | [0, 5000] | bid-depth floor: 0 = off; 5000 = top-decile demand, same family as flash_arb. |
| `min_ask_depth` | decimal | `300` | [0, 5000] | ask-depth floor (the exit side's liquidity): same reasoning as min_bid_depth. |
| `max_abs_obi` | decimal | `0.4` | [0.05, 1] | one-sided-book veto: same semantics as flash_arb/mad_dog. |
| `trend_window_sec` | int | `30` | [5, 300] | chip-calm window (also the mid-ring retention): 5 = minimum real coverage; 300 = a third of the round — a 'trend' that long is the round's whole shape. |
| `trend_max_move_pct` | decimal | `5` | [0.5, 50] | max tolerated chip decline over the window: 0.5 = near-absolute calm demanded (strategy nearly off, the paranoid extreme); 50 = a halving tolerated — the veto never fires on mid-band chips. |
| `spot_window_sec` | int | `30` | [5, 120] | underlying-calm window: same family as mad_dog's spot_window_sec. |
| `spot_max_move_pct` | decimal | `1.5` | [0.1, 5] | contrary-spot veto threshold: same family as mad_dog's spot_max_move_pct. |
| `min_available_usd` | decimal | `1.00` | [0, 1000] | available-balance pre-filter: same family as flash_arb/mad_dog. |

`take_profit_spread` is the strategy's ONE exit suggestion (banking a banked-spread
profit at `mid >= bid + spread`, kernel-priced and kernel-adjudicated like every
exit) — a profit-taking target, not a stop: it never widens a loss, and the kernel's
own ladder (protective stop, time force-exit, trailing) runs underneath it
regardless. It is a magnitude with a real domain; declaring it evolvable does not
hand survival to the strategy.

## flash_arb

Spot-to-venue lag catcher: a fast spot move buys the implied side post_only at the stale ask, inside the freshness window.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `mom_window_sec` | int | `2` | [1, 30] | spot momentum window over closed sec1 bars: 1 = a single bar (fastest real coverage); 30 = a 'flash' that is already a trend — the venue reprices well inside 30s and the lag premise dies. |
| `mom_threshold_pct` | decimal | `0.25` | [0.05, 2] | \|spot move\| % inside the window: 0.05 = the noise floor (sub-spread moves trigger on microstructure, spamming the kernel with non-signals); 2% inside seconds is flash-crash scale — the venue has already repriced, there is no lag left to join. |
| `lag_max_ms` | int | `1500` | [1000, 10000] | trigger freshness: 1000 = one sec1 bar cadence (a trigger is seen at a bar close — a freshness window shorter than the cadence is structurally near-dead); 10000 = ten seconds stale is not a lag, the premise is fantasy by then. |
| `lag_max_price` | decimal | `0.55` | [0.10, 0.95] | ceiling on the ask paid for the lag side: 0.10 is the ledger's lottery boundary (sub-0.10 legs are 4.5% win-rate tickets, not lags); 0.95 = residual upside <= 0.05 cannot pay fees+slippage. |
| `min_time_left_sec` | int | `180` | [0, 300] | entry time floor: 0 defers to the kernel gate; 300 = only the back half of a 900s round — beyond it the always-on catcher becomes a window sniper. |
| `min_bid_depth` | decimal | `500` | [0, 5000] | bid-depth floor: 0 = gate off (only a negative depth fails, i.e. never); 5000 = top-decile depth demand that turns the gate into an off-switch on ordinary books. |
| `max_abs_obi` | decimal | `0.3` | [0.05, 1] | one-sided-book veto: 0.05 demands near-perfect balance (real books rarely sit that flat -> strategy nearly off); 1 = the veto can never fire (obi in [-1,1]) — the widest sane allowance. |
| `min_available_usd` | decimal | `1.00` | [0, 1000] | available-balance pre-filter: 0 = off; 1000 = 1% of the 100k seed balance — a throttle the kernel's own funds path already owns; this gate is meant to stop entries into an empty account, not to size. |

## hot_side_momentum

Confirmed-leader holder: round 40-70% through, spot momentum confirms the higher-asked side, buy it and hold to settlement.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `round_sec` | int | `900` | [300, 1800] | round-length passthrough (the progress window is computed against it): 300 = the 5m rounds the venue lists (shortest); 1800 = 30m (longest). Outside that range the package describes rounds the venue does not run. |
| `mom_window_sec` | int | `15` | [5, 120] | spot momentum window: 5 = below that a 'confirmed' move is 1-2 prints of noise (this strategy needs confirmation, not flashes — that is flash_arb's job); 120 = two minutes, at which point the window is a third of the round and the 'impulse' is just the round's own history. |
| `mom_threshold_pct` | decimal | `0.10` | [0.05, 1] | \|move\| % inside the window: 0.05 = noise floor over a 15s span; 1% inside 15s is flash-scale — a confirmation that strong is a repricing, not a leader confirmation. |
| `mom_max_age_ms` | int | `2000` | [1000, 15000] | trigger staleness bound: 1000 = one bar cadence; 15000 = a 15s-old trigger at the default 15s window means the window has fully rolled over — the confirmation describes the previous window entirely. |
| `min_leading_price` | decimal | `0.60` | [0.50, 0.75] | leader-band floor (leader = strictly higher ask): 0.50 = never call a coin-flip side 'leading' (the ledger prices 0.40-0.50 at ~zero EV); 0.75 keeps a non-degenerate band against max_leading_price's default. |
| `max_leading_price` | decimal | `0.80` | [0.65, 0.95] | leader-band ceiling: 0.65 keeps the band >= 0.05 wide against min_leading_price's default; 0.95 = the residual-upside boundary (>= 0.95 legs cannot pay fees on a hold-to-settlement naked leg). |

## mad_dog

Panic-wick catcher: after dominance holds, a fast wick below broken_price arms a resting maker bid wick_offset under the low.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `dominant_min_price` | decimal | `0.55` | [0.50, 0.80] | mid level a side must have held to count as dominant: 0.50 = a side below even odds is not dominant; 0.80 = self-evident dominance that makes the pre-wick setup nearly unsatisfiable alongside a 0.35 broken zone. |
| `dominant_hold_sec` | int | `20` | [5, 300] | hold-spell length for dominance spelling (a): 5 = five consecutive seconds of book prints (250ms buckets give real resolution); 300 = a third of the round held calm before the wick — the strictest spell that still leaves time to trade. |
| `dominant_mean_sec` | int | `60` | [10, 300] | rolling-mean window for dominance spelling (b): 10 keeps the window above the hold-spell domain floor (the coverage check spans dominant_hold_sec); 300 = a third of the round. |
| `broken_price` | decimal | `0.35` | [0.10, 0.50] | wick-zone threshold (mid below it = broken): 0.10 = the ledger's lottery floor — below it a 'wick' is a terminal repricing, not a panic wick; 0.50 overlaps dominance itself (a 0.50 mid is both 'was dominant at 0.55' and 'now broken' — the identity blurs). |
| `dip_speed_pct` | decimal | `15` | [1, 50] | % drop into the broken zone required inside dip_speed_sec: 1 = one tick's worth at mid-band prices (the smallest measurable panic); 50 = a halving — the extreme end (a drop into a <=0.35 zone from a <=1.00 ref tops out at 65%, so beyond 50 only the most violent repricings in the venue's history qualify). |
| `dip_speed_sec` | int | `3` | [1, 30] | speed window: 1 = the fastest real measurement (250ms buckets); 30 = a 30s 'speed' is a decline, not a panic — drift is chip_calm/market_maker territory. |
| `spot_max_move_pct` | decimal | `0.5` | [0.1, 5] | veto threshold for a contrary spot move: 0.1 = below BTC's 10s noise (veto nearly always fires -> strategy off, the 'paranoid' extreme); 5 = a 5% adverse move in 10s is crash mode regardless — beyond it the veto is academic. |
| `spot_window_sec` | int | `10` | [5, 120] | underlying-calm window over sec1 bars: 5 = minimum real coverage (span >= window-1); 120 = two minutes — drift measurement again, not calm. |
| `min_time_left_sec` | int | `180` | [0, 300] | entry time floor: 0 defers to the kernel gate; 300 = late-window sniper boundary (the resting bid needs time to fill). |
| `min_bid_depth` | decimal | `500` | [0, 5000] | bid-depth floor: 0 = off; 5000 = top-decile depth demand (off-switch territory), same reasoning as flash_arb. |
| `max_abs_obi` | decimal | `0.5` | [0.05, 1] | one-sided-book veto: 0.05 = near-perfect balance demanded; 1 = veto can never fire, same semantics as flash_arb. |
| `min_available_usd` | decimal | `1.00` | [0, 1000] | available-balance pre-filter: 0 = off; 1000 = 1% of seed — kernel funds path owns the real gate. |
| `wick_offset` | decimal | `0.02` | [0, 0.20] | resting-bid distance under the wick low: 0 = bid AT the low (most aggressive placement, still maker — the entry < ask guard holds); 0.20 under a <=0.35 broken-zone low lands the bid in the <=0.15 lottery zone, and recovery (mid back above broken_price) cancels the bid anyway. |

## oracle_ruler

Calibration probe (not a strategy): one unconditional maker bid at entry_price per round; any booked PnL disagreeing with the closed form is a measurement defect.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `entry_price` | decimal | `0.30` | [0.05, 0.60] | the probe's single resting bid: the probe must stay a probe — below 0.05 the bid may never fill on ordinary books and the ruler silently measures nothing; above the prevailing ask it degrades from a maker probe to a taker probe (the fill-mechanics assertion dies), and 0.60 bounds that degradation zone while leaving the measurement well-defined. |

## lua_momentum

Official Lua example: per-token momentum follower demonstrating the suggested surface.

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `threshold` | decimal | `0.04` | [0.005, 1] | per-tick mid move % that classifies momentum (the official example's one tunable): 0.005 = microstructure noise (direction flips every tick — the demo stops demonstrating momentum); 1 = a single-tick 1% move is extreme repricing only — the demo stops demonstrating altogether. Both bounds keep the example an example. |

## kline_probe

Throwaway replay probe (flash_arb's latch verbatim, gates off): observable via order counts and reason text.

The manifest previously declared no tunables (the probe is a throwaway diagnostic);
its `strategy.lua` nevertheless reads `mom_window_sec`/`mom_threshold_pct` with
hardcoded fallbacks. Both are now declared with defaults EQUAL to those fallbacks,
making the probe evolvable without changing any behavior — the other bisect gates
stay hardcoded OFF (that is the probe's point).

| tunable | type | default | domain | why these bounds |
|---|---|---|---|---|
| `mom_window_sec` | int | `2` | [1, 30] | spot momentum window for the probe's flash_arb-verbatim latch: same family as flash_arb's mom_window_sec (1 = one bar; 30 = trend, not flash). |
| `mom_threshold_pct` | decimal | `0.10` | [0.05, 2] | \|move\| % inside the window: same family as flash_arb's mom_threshold_pct (0.05 = noise floor; 2 = flash scale). |

## How to turn evolution on

```
blitzkrieg-core --shadow-evolution \
  [--se-min-samples N] [--se-eval-window-secs N] [--se-min-obs-secs N] \
  [--se-cooldown-secs N] [--se-variant-count N] [--se-auto-evolve true|false] \
  [--se-cycle-secs N] [--se-ttl-secs N] [--se-deep-dims N] [--se-audit-dir DIR]
```

- Units register when strategies load (`rewire_hot_params`); each unit gets a shadow
  twin set under `VariantSet`. `--se-auto-evolve false` (default) holds every
  qualifying variant as an E13 proposal for operator accept/reject/defer.
- Audit trail: `<audit_dir>/<strategy>.jsonl` per strategy (default
  `data/evolution/`), plus `proposals.jsonl` / `promotions.jsonl` there.
- The persisted runtime switch (state.json / IPC `shadow_evolution.set_auto`) wins
  over the startup flag where they disagree — the startup report names the source.
- Short replays need the tuning flags pulled down (e.g. `--se-min-samples 2
  --se-min-obs-secs 0 --se-cooldown-secs 0 --se-eval-window-secs 60`) to see a
  proposal inside the window; smoke evidence for this PR lives in the issue thread.

## Offline calibration (this PR's Task B, honest-numbers summary)

Protocol: the frozen 15m corpus (348,631 events) has NO spot/kline events, so
five of the ten packages are structurally silent on it (flash_arb,
hot_side_momentum, kline_probe — kline triggers; mad_dog, market_maker —
`spot_missing = block` vetoes every entry). The remaining five were swept on a
by-round 60/40 train/val split of the head corpus (24 replay arms, ~80 s each,
raw rows + per-arm reports in the agent work area): single-knob perturbations
of every high-leverage knob (pair_discount_arb max_pair_cost 0.985..0.999,
min_time_left_sec 60; single_leg_pair min_gap 0.01/0.03, max_open_positions
2/5, min_time_left_sec 90; spread_arb trend_confirm_sec 40/90,
trend_entry_factor 0.85/0.92, trend_max_entry_price 0.40/0.50/0.55;
oracle_ruler entry_price 0.10..0.50; lua_momentum threshold 0.02/0.10).

Verdicts, per the +3%-and-val-same-direction bar:

| strategy | verdict |
|---|---|
| pair_discount_arb | every probed point LOSES on the train slice (best runner-up −15% net) — default optimal |
| single_leg_pair | every probed point loses (min_gap 0.01 collapses trades 31→5; min_time_left_sec 90 is inert) — default optimal |
| spread_arb | zero trades in EVERY probed configuration on this corpus (the entry formula binds: confirm needs mid >= 0.55, the resting bid never gets lifted) — no calibration signal exists; defaults unchanged |
| market_maker | structurally silent here (spot gate) — nothing to calibrate on frozen corpora; domains enable runtime evolution instead |
| flash_arb | structurally silent here (no klines) — same |
| hot_side_momentum | structurally silent here — same |
| mad_dog | structurally silent here — same |
| kline_probe | diagnostic probe, silent here — same |
| oracle_ruler | NOT calibratable by design: entry 0.30 is the measurement contract oracle-ruler-check.mjs asserts closed-form PnL against; perturbations only make the probe lose less, which is meaningless |
| lua_momentum | zero signals on this corpus — nothing to calibrate |

**Zero defaults changed** — no candidate cleared the bar, so nothing was
shortlisted and no full-corpus arbitration run was owed. That is the honest
outcome: the value of this PR is the DOMAINS (runtime evolution now possible
everywhere), not a hardcoded tuning win. If a knob looks tempting anyway,
that is exactly what the machinery this PR opts strategies into is for.

## Verification recipe

```
# sha256 (entry-file digest, plain SHA-256 of strategy.lua; lua_loader refuses a mismatch):
for d in user_layer/strategies_lua/*/; do \
  n=$(basename $d); \
  c=$(python3 -c "import json;print(json.load(open('$d/manifest.json'))['sha256'])"); \
  a=$(shasum -a 256 $d/strategy.lua | cut -d' ' -f1); \
  [ "$c" = "$a" ] && echo "$n OK" || echo "$n MISMATCH"; done

# gate (charter §16.4):
node scripts/strategy-no-stop-loss-check.mjs
```
