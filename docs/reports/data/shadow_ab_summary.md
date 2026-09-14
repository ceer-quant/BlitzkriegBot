# Shadow Evolution A/B — raw summary
groups: A=evolution OFF, B=evolution ON (identical tick stream, params, balance)
assets_per_round: 70, round_sec: 86400, tick_ms: 4000, scripted_rounds: 1

## Realised (dry) metrics
group,trades,wins,win_rate,pf_ratio,net_pnl,max_dd
A,70,60,0.8571,4.0947,78.3600,2.5320
B,70,60,0.8571,4.0947,78.3600,2.5320

## Group B evolution
evolutions_applied: 0
evolutions_rejected_natural: 0
audit_records: 0
safety_lock_probe: REJECTED by Lock 1: gradient too large for trend_max_entry_price: 0.20 > 0.05
final_params_vs_initial_max_entry_cap: 0.45 -> 0.45

## Group B parameter trajectory (changed fields only)
ts,field,from,to,reason,applied,rejection

## EXP-C: manager-level qualification (real ShadowEvolution, no engine lag)
baseline: n=48 wr=0.7500 pf=1.8078 pnl=24.5456
best_variant: n=36 wr=1.0000 pf=100.0000 pnl=54.9296
evolutions_applied: 1
evolutions_rejected: 0
final_cap: 0.4410  final_min_price: 0.5390
trajectory:
  applied=true reason=CombinedImprovement trend_min_price:0.55->0.5390, trend_entry_factor:0.98->0.9604, trend_max_entry_price:0.45->0.4410, trend_broken_price:0.35->0.3430
