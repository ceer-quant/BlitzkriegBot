# late_favorite

Buy the BTC side priced 0.80–0.90 while 45–120 seconds remain, then hold
it to settlement. One attempt per condition per round.

The registry id is `late_favorite`. The loader does not require a `lua_`
prefix; the directory name must equal `manifest.name`.

This is the configuration that cleared an untouched settlement window
(2026-09-26 through 2026-09-30): 351 trades, 97.72% win rate, profit
factor 6.741, net +382.40, max drawdown 15.67, after
`10 × 0.07 × ask × (1−ask)`, redemption fee 0. Those are backtest numbers.
Live results can differ. `DRY_RUN` stays the process default. The package
is not in the default enabled set. Enable it explicitly:

```sh
--enable-strategy late_favorite
```

| tunable | default | meaning |
|---|---|---|
| `btc_only` | 1 | BTC only. Set 0 to include ETH, SOL and XRP |
| `maker_entry` | 0 | 0 = taker at the ask. 1 = post-only bid, cancelled once time left falls below `min_time_left` |
| `min_px` / `max_px` | 0.80 / 0.90 | inclusive band |
| `min_time_left` / `max_time_left` | 45 / 120 | seconds remaining |

The all-asset taker book also cleared that four-day window (2,740 trades,
87.88%, profit factor 1.208) and missed a previously examined window
(2026-10-19 through 2026-10-26, profit factor 1.182). The maker book lost
money there (profit factor 0.829). Details:
`docs/late-favorite.md` and
`docs/reports/lua-momentum-holdout-2026-09.md`.

`bk_thesis_death` returns false. The thesis is the redemption. An
intra-round bid drop is the variance the entry price already charges.
Timing and spot-momentum gates are waived down to 45 seconds so the
scanner's 180-second floor does not reject every order. Risk limits still
apply.
