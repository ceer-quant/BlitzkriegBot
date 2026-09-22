#!/usr/bin/env node
/**
 * blitzkrieg-new-strategy — E9-a (#60) deliverable 1.
 *
 * Generates a complete, buildable Blitzkrieg strategy crate under
 * `user_layer/strategies/<name>/` (its own nested workspace, exactly like the
 * dog example). The generated code contains ZERO unsafe: the business logic
 * lives in `impl SafeStrategy`, and `blitzkrieg-strategy-api`'s
 * `export_strategy!` macro emits the whole C ABI v2 surface (vtable, factory,
 * `#[no_mangle]` exports, JSON envelopes).
 *
 * Usage:
 *   node scripts/blitzkrieg-new-strategy.mjs my_dip_fade
 */
import { writeFileSync, mkdirSync, existsSync } from 'fs';
import { join, resolve } from 'path';

const name = process.argv[2];
if (!name || !/^[a-z][a-z0-9_]*$/.test(name)) {
  console.error(
    'usage: blitzkrieg-new-strategy.mjs <name>  (rust_snake_case, e.g. my_dip_fade)'
  );
  process.exit(2);
}
const dir = join(resolve(process.cwd()), 'user_layer', 'strategies', name);
mkdirSync(join(dir, 'src'), { recursive: true });
const librs = join(dir, 'src', 'lib.rs');
if (existsSync(librs)) {
  console.error(`refusing to overwrite existing ${librs}`);
  process.exit(1);
}

const camel = name.split('_').map((s) => s[0].toUpperCase() + s.slice(1)).join('');
const lib = `//! Blitzkrieg strategy "${name}" — generated E9-a template (C ABI v2).
//!
//! This file is the ENTIRE strategy. The vtable, FFI exports and JSON glue
//! are owned by \`blitzkrieg-strategy-api\`'s \`export_strategy!\` — there is no
//! \`unsafe\` anywhere in a generated strategy.
//!
//! The three things you edit, in the order you'll touch them:
//!   1. \`on_book\`    — observe: cache anything you'll decide on.
//!   2. \`evaluate\`   — decide: emit entry/exit intents (the kernel sizes,
//!                      prices closes, and owns everything risk-related).
//!   3. \`on_params\` / \`evolvable_knobs\` — tune: hot-swappable parameters and
//!      the declaration that lets shadow evolution vary your own logic.

use blitzkrieg_strategy_api::{
    dec, export_strategy, BookUpdate, Entry, Intents, Knob, MarketInfo, ParamBag, RoundContext,
    RoundInfo, SafeStrategy,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Entry intents use LIMIT prices; the kernel validates against the live
/// book, sizes, reserves and submits — a strategy never sees a signer or
/// socket, and everything still passes the kernel's risk gates.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    buy_below: Decimal,   // enter when mid <= this
    min_depth: Decimal,   // bid depth needed to trust the dip
    max_buys: usize,      // per-evaluate cap, keeps the template boring
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buy_below: dec!(0.40),
            min_depth: dec!(50),
            max_buys: 1,
        }
    }
}

/// Round used by the offline smoke test.
pub fn template_round() -> RoundInfo {
    RoundInfo { slot: 0, time_left_sec: 600, now_ms: 0 }
}

/// Market used by the offline smoke test.
pub fn template_market() -> MarketInfo {
    MarketInfo {
        asset: "BTC".into(),
        condition_id: "0xc".into(),
        up_token: "UP".into(),
        down_token: "DOWN".into(),
        expires_at_ms: 0,
        slot: 0,
        neg_risk: false,
    }
}

/// The strategy struct: plain Rust, no unsafe, no FFI.
#[derive(Default)]
struct ${camel}Strategy {
    cfg: Config,
    /// Last book per symbol — the strategy's whole working memory.
    books: std::collections::HashMap<String, BookUpdate>,
}

impl SafeStrategy for ${camel}Strategy {
    /// REQUIRED — the name operators see in strategy.list / TUI Plugins.
    fn name(&self) -> &str {
        "${name}"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    /// REQUIRED — called at feed rate; keep it cheap (cache, don't decide).
    fn on_book(&mut self, u: &BookUpdate) {
        if u.mid.is_some() {
            self.books.insert(u.symbol.clone(), u.clone());
        }
    }

    /// REQUIRED — the decision hook. Runs once per engine evaluation.
    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        let mut intents = Intents::none();
        let mut buys = 0;
        for m in &ctx.markets {
            for token in [&m.up_token, &m.down_token] {
                if buys >= self.cfg.max_buys {
                    return intents;
                }
                let Some(book) = self.books.get(token) else { continue };
                // Numbers arrive as the ABI's decimal STRINGS; \`dec()\` parses
                // them EXACTLY (rust_decimal) — never convert through f64, or
                // a resting price can drift by a tick.
                let (Some(mid), Some(bid_depth)) = (dec(&book.mid), dec(&book.bid_depth)) else {
                    continue;
                };
                // THE TEMPLATE RULE — replace with your own logic. Everything
                // you need is in the cached BookUpdate (mid, ladder depths,
                // obi, spread) and the round context (timing, markets).
                if mid <= self.cfg.buy_below && bid_depth >= self.cfg.min_depth {
                    intents.entries.push(Entry {
                        token: token.clone(),
                        price: book
                            .best_ask
                            .clone()
                            .unwrap_or_else(|| mid.to_string()),
                        reason: "${name}_dip".into(),
                        // \`shares: None\` = the kernel's notional sizing. Set
                        // \`Some(n.to_string())\` to ask for an explicit share
                        // count instead (clamped to the kernel's risk band) —
                        // that is how the legs of a pair are made to match.
                        shares: None,
                    });
                    buys += 1;
                }
                // TODO: position management goes here. While holding a token
                // (you decide when: track it in the struct), push an Exit:
                //   intents.exits.push(Exit { token, reason: "tp" });
            }
        }
        intents
    }

    /// Hot-swappable params. Return false to REJECT the whole bag (the
    /// kernel keeps the previous one). Host config and shadow-evolution
    /// proposals both arrive here, same path.
    fn on_params(&mut self, p: &ParamBag) -> bool {
        let mut ok = true;
        if let Some(v) = p.get_dec("buy_below") {
            if (dec!(0.05)..=dec!(0.90)).contains(&v) {
                self.cfg.buy_below = v;
            } else {
                ok = false;
            }
        }
        if let Some(v) = p.get_dec("min_depth") {
            if (Decimal::ZERO..=Decimal::from(1_000_000_000)).contains(&v) {
                self.cfg.min_depth = v;
            } else {
                ok = false;
            }
        }
        ok
    }

    /// Declaring knobs = shadow evolution can vary this strategy's own
    /// logic (E2-c). No knobs = simply not evolvable.
    fn evolvable_knobs(&self) -> Vec<Knob> {
        vec![
            Knob {
                name: "buy_below".into(),
                value: self.cfg.buy_below.to_string(),
                min: "0.05".into(),
                max: "0.90".into(),
            },
            Knob {
                name: "min_depth".into(),
                value: self.cfg.min_depth.to_string(),
                min: "0".into(),
                max: "1000000".into(),
            },
        ]
    }

    /// Tokens currently considered entry-eligible (drives the kernel's
    /// confirmation accounting; empty is fine for a trigger-on-evaluate
    /// template like this one).
    fn confirmed_tokens(&self) -> Vec<String> {
        self.books
            .iter()
            .filter(|(_, b)| dec(&b.obi).is_some_and(|o| o > Decimal::ZERO)
                && b.bid_levels >= 2 && b.ask_levels >= 2)
            .map(|(t, _)| t.clone())
            .collect()
    }

    /// Show up in diagnostics listings (kernel aggregates whatever you emit).
    fn diagnostics(&self) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "config": { "buy_below": self.cfg.buy_below.to_string(), "min_depth": self.cfg.min_depth.to_string() },
                "tokens_seen": self.books.len(),
            })
        ]
    }
}

// One line plugs the struct into the full C ABI v2.
export_strategy!(crate::${camel}Strategy);

#[cfg(test)]
mod tests {
    use super::*;

    /// The template rule must be executable without a kernel: feed a dip
    /// book and expect exactly one entry intent (buildable+testable now,
    /// smoke-tested against the real kernel by \`node scripts/strategy-devcheck.mjs\`).
    #[test]
    fn buys_the_dip() {
        let mut s = ${camel}Strategy {
            cfg: Config { buy_below: dec!(0.40), min_depth: dec!(50), max_buys: 1 },
            books: Default::default(),
        };
        s.on_book(&BookUpdate {
            symbol: "UP".into(),
            asset: "BTC".into(),
            best_bid: Some("0.38".into()),
            best_ask: Some("0.39".into()),
            mid: Some("0.39".into()),
            bid_depth: Some("100".into()),
            ask_depth: Some("90".into()),
            obi: Some("0.1".into()),
            spread: Some("0.01".into()),
            spread_pct: Some("2.5".into()),
            timestamp_ms: 1,
            bid_levels: 3,
            ask_levels: 3,
        });
        let ctx = RoundContext {
            round: crate::template_round(),
            markets: vec![crate::template_market()],
        };
        let out = s.evaluate(&ctx);
        assert_eq!(out.entries.len(), 1, "{out:?}");
        assert_eq!(out.entries[0].token, "UP");
        assert_eq!(out.entries[0].price, "0.39");
        // Shallow depth -> no trade.
        s.books.get_mut("UP").unwrap().bid_depth = Some("10".into());
        assert!(s.evaluate(&ctx).entries.is_empty());
    }
}
`;
writeFileSync(librs, lib);

writeFileSync(
  join(dir, 'Cargo.toml'),
  `# Generated by blitzkrieg-new-strategy (E9-a / #60). A nested workspace, like
# a real third-party strategy author would have: own lockfile, detached from
# the kernel's build graph.
[package]
name = "${name}"
version = "0.1.0"
edition = "2024"

[lib]
name = "${name}"
crate-type = ["cdylib", "rlib"]

[dependencies]
blitzkrieg-strategy-api = { path = "../../strategy_api" }
rust_decimal = "1"
rust_decimal_macros = "1"
serde_json = "1"

[workspace]
`
);

writeFileSync(
  join(dir, 'README.md'),
  `# ${name}

Generated by \`node scripts/blitzkrieg-new-strategy.mjs ${name}\` (E9-a / #60).

## Verify it end-to-end (kernel in the loop)
\`\`\`bash
node scripts/strategy-devcheck.mjs
\`\`\`
This builds the crate, loads the \`strategy.load\` dylib into a real dry core,
asserts it registered DISABLED, enables it, drives books through the engine
and asserts intents/params/knobs are visible in \`engine.stats\`.

## One-command demo against a live core
\`\`\`bash
cd user_layer/strategies/${name} && cargo build --release
# target/release/lib${name}.{dylib,so} — load via TUI command bar:
#   strategy.load /full/path/to/lib${name}.dylib
\`\`\`

## Edit
Only \`src/lib.rs\`, three TODOs: observe (\`on_book\`) → decide (\`evaluate\`) →
tune (\`on_params\`/\`evolvable_knobs\`). Zero unsafe. New strategies always start
DISABLED.
`
);

console.log(`created ${dir}`);
console.log('next: node scripts/strategy-devcheck.mjs   (builds it and verifies against a real dry core)');
