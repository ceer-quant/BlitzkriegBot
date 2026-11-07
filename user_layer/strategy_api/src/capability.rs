//! The frozen strategy capability model (issue #843, P1) — ONE declaration
//! surface every runtime spells identically.
//!
//! Strategy capabilities today speak two dialects of the same vocabulary: the
//! C ABI exposes them as OPTIONAL SYMBOLS (`bk_strategy_trailing_exit`,
//! `bk_strategy_atomic_batch`, …) and their wire payload keys; a Lua package
//! speaks the same capabilities as `manifest.json` KEYS (`trailing_exit`,
//! `atomic_batch`, …) plus the entry-point names. Neither side named the
//! shared vocabulary, so a new capability could land on one runtime and not
//! the other, and the only proof of agreement was a doc search.
//!
//! This module is the single source for that shared vocabulary (the entity
//! Carla the issue text calls the "capability conformance surface"). It
//! carries only NAMES, NUMBERS and a declaration TABLE — no behaviour, no
//! I/O, nothing that can drift between crates:
//!
//! * [`STRATEGY_CAPABILITY_MODEL_VERSION`] / [`MANIFEST_API_VERSION`] — the
//!   version constants (the manifest gate and the ABI both anchor on "1.0");
//! * [`GateExemptionKey`] / [`GateExemption`] — the entry-gate exemption
//!   names, one table for the C-ABI JSON payload, the Lua manifest keys and
//!   the core's `GateExemptions` fields (they were three hand-copies of the
//!   two same words);
//! * [`TrailingExitVerdict`] — the three-state verdict with THE ABI encoding
//!   (`-1`/`0`/`1`), so the `i32` read in `foreign.rs` and the
//!   `Option<bool>` in a manifest are one conversion away from the same
//!   enum, never two spellings;
//! * [`DeclCap`] + [`DECLARATION_MAP`] — the declaration-crossing table:
//!   every capability a strategy may DECLARE, with its C-ABI symbol (or
//!   `None` for the vtable-native hooks), its Lua manifest key and the
//!   `EngineStrategy` trait method it lands on. A new capability that does
//!   not appear here does not exist on either runtime (the nursery test
//!   iterates the map and demands the symbol constant compiles — a typo in
//!   a spelling is a red test, not a silent gap);
//! * [`LuaEntryPoint`] — the four `bk_*` entry names (validated against the
//!   runtime's own `ENTRY_POINTS` table so the two cannot drift);
//! * [`StrategyCapabilityGrant`] — the host-created, immutable capability
//!   GRANT the issue's scope item 2 asks for: account, venues, tokens,
//!   round-period and the data-surface visibility. A grant is built by the
//!   host (`host_grant`, the current shipped posture), frozen thereafter,
//!   and its defaults are FAIL-CLOSED (nothing visible) — a runtime that
//!   forgets to project a grant out of thin air does not widen what a
//!   strategy sees.
//!
//! ### What is deliberately NOT here
//!
//! The sandbox quotas (16 MiB / 1e6 instructions) live in
//! `blitzkrieg_lua_runtime::sandbox` and STAY there: they are §6.2/§6.3
//! enforcement constants behind a Lua VM, not declaration vocabulary, and
//! the issue's freeze is about the declaration surface. The map below
//! carries their names only as far as the declaration rows reach.
//!
//! ### Versioning
//!
//! `BK_ABI_VERSION` stays 2 (the wire layout is frozen by the golden test
//! `abi_layout.rs`). This model versions SEPARATELY: adding a row to
//! [`DECLARATION_MAP`] is a `STRATEGY_CAPABILITY_MODEL_VERSION` bump with a
//! disclosed PR line, never an ABI bump — the same rule the optional-symbol
//! precedent already established for the C side.

/// The frozen version of this capability model. Rows may be APPENDED (with
/// the version bump and a disclosed migration note, per #843's
/// "versioned adapter" clause); existing rows' spellings never change —
/// a rename here would re-open the exactly the drift this module closes.
pub const STRATEGY_CAPABILITY_MODEL_VERSION: u32 = 1;

/// The one supported `manifest.json` API version (DEV_V0_3 §6.4). The Lua
/// loader's `api == "1.0"` gate anchors HERE, so the manifest spec and the
/// C ABI's declared "BlitzkriegStrategy API **1.0**" formal name can never
/// disagree. The value is `pub` because the core's loader (the validator)
/// and this crate (the declaration side) both name it.
pub const MANIFEST_API_VERSION: &str = "1.0";

// ── entry-gate exemptions (E2-b / #27; three runtimes, two words) ───────────
//
// The exemption NAME is the wire spelling in every channel that carries one:
// the C-ABI JSON payload `{"timing":b,"momentum":b[, "timing_min_time_left_sec":n]}`,
// the Lua manifest's `gate_exemptions` object, and the core's `GateExemptions`
// record keys ("豁免门禁 Y" audit lines). One table.

/// A declared entry-gate exemption, in the shared wire spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GateExemption {
    /// Waive the round-timing window (`timing`).
    Timing,
    /// Waive the spot momentum alignment filter (`momentum`).
    Momentum,
}

impl GateExemption {
    /// All declared exemptions, in the map/table order every other spelling
    /// follows (the JSON key order and the manifest key order agree with
    /// this). Declared as data so a new exemption extends in ONE place.
    pub const ALL: [GateExemption; 2] = [GateExemption::Timing, GateExemption::Momentum];

    /// The wire key this exemption is named by on BOTH runtimes.
    pub fn key(self) -> &'static str {
        match self {
            GateExemption::Timing => GATE_TIMING_KEY,
            GateExemption::Momentum => GATE_MOMENTUM_KEY,
        }
    }

    /// The stable `gate_exemptions` audit-line name (the core's
    /// `GateExemptions::gates` order). Identical to [`Self::key`] today and
    /// spelled separately so an audit-line change and a wire change cannot
    /// silently become one decision.
    pub fn audit_name(self) -> &'static str {
        self.key()
    }
}

/// The C-ABI JSON key AND Lua manifest key of the `timing` exemption.
pub const GATE_TIMING_KEY: &str = "timing";
/// The C-ABI JSON key AND Lua manifest key of the `momentum` exemption.
pub const GATE_MOMENTUM_KEY: &str = "momentum";
/// The optional floor key (D-31): shared by both payload spellings.
pub const GATE_TIMING_FLOOR_KEY: &str = "timing_min_time_left_sec";

// ── the three-state trailing verdict (issue 356 / #642) ────────────────────

/// A strategy's own trailing-stop verdict, the ONE enum the `i32` ABI
/// symbol and the manifest's `trailing_exit` boolean both resolve into.
///
/// Both channels carry TWO states and the kernel carries the third: an
/// absent symbol / absent manifest key is [`TrailingExitVerdict::FollowKernel`]
/// (the `Option<bool>` `None`), `1`/`true` is [`TrailingExitVerdict::Trail`],
/// `0`/`false` is [`TrailingExitVerdict::Never`]. Any other `i32` reads as
/// "no declaration" — the fail-closed direction the C side already pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrailingExitVerdict {
    /// No declaration — follow the kernel-wide `risk.setExit trailingEnabled`
    /// value (the ABI symbol's `-1`; the manifest's absent key).
    FollowKernel,
    /// Bank profit on give-back even when the kernel value is off
    /// (`1` / `true`).
    Trail,
    /// Never trail even when the kernel value is on (`0` / `false`).
    Never,
}

impl TrailingExitVerdict {
    /// The `i32` the `bk_strategy_trailing_exit` symbol returns.
    pub const UNDECLARED_ABI: i32 = -1;
    /// The `i32` for [`TrailingExitVerdict::Trail`].
    pub const TRAIL_ABI: i32 = 1;
    /// The `i32` for [`TrailingExitVerdict::Never`].
    pub const NEVER_ABI: i32 = 0;

    /// Decode the ABI symbol's return. Any value outside `{-1, 0, 1}` reads
    /// as [`TrailingExitVerdict::FollowKernel`] — the pre-existing
    /// fail-closed rule, now stated by the shared type.
    pub fn from_abi_int(v: i32) -> Self {
        match v {
            Self::TRAIL_ABI => Self::Trail,
            Self::NEVER_ABI => Self::Never,
            _ => Self::FollowKernel,
        }
    }

    /// The ABI symbol value to return for this verdict.
    pub fn to_abi_int(self) -> i32 {
        match self {
            Self::FollowKernel => Self::UNDECLARED_ABI,
            Self::Trail => Self::TRAIL_ABI,
            Self::Never => Self::NEVER_ABI,
        }
    }

    /// Decode the Lua manifest's `trailing_exit` (an absent key = `None` =
    /// follow the kernel). Kept total over `Option<bool>` so the manifest
    /// parser and the symbol reader convert through the same fn.
    pub fn from_manifest(v: Option<bool>) -> Self {
        match v {
            None => Self::FollowKernel,
            Some(true) => Self::Trail,
            Some(false) => Self::Never,
        }
    }

    /// The core-facing `Option<bool>` (`None` = follow the kernel value).
    pub fn to_option_bool(self) -> Option<bool> {
        match self {
            Self::FollowKernel => None,
            Self::Trail => Some(true),
            Self::Never => Some(false),
        }
    }
}

// ── the declaration map: C-ABI symbol ↔ Lua manifest key ↔ trait method ───

/// One capability a strategy may DECLARE, crossing both runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclCap {
    /// The three-layer market-mode declaration (§2.3 / §7) — `modes`.
    Modes,
    /// Entry-gate exemption opt-out (E2-b) — `gate_exemptions`.
    GateExemptions,
    /// Evolvable-knob declaration (E2-c) — `evolvable_knobs` (C: the JSON
    /// array payload symbol; Lua: the manifest `tunables` domains).
    EvolvableKnobs,
    /// Fresh-book binder (E-parity) — C-only: `bind_eval_ctx` (a Lua package
    /// receives the same freshness verdict through `bk.book(...).fresh`).
    BindEvalCtx,
    /// Effective-config reporter (observability) — `config_view`.
    ConfigView,
    /// Hold-to-settlement posture — `holds_to_settlement`.
    HoldsToSettlement,
    /// The OPTIONAL closed-bar hook (§10.4) — `on_kline`.
    OnKline,
    /// Pair-declaration default (RFC 568) — `entry_pair`.
    EntryPair,
    /// Three-state trailing verdict — `trailing_exit`.
    TrailingExit,
    /// Atomic-batch declaration (E-VA) — `atomic_batch`.
    AtomicBatch,
    /// Entry fill-policy doctrine (`entry_maker`): Lua manifest key; the C
    /// ABI reaches the same posture through `evaluate` intent JSON + the
    /// host's adopt path, so its C row is `None` (no own symbol yet).
    EntryMaker,
}

/// One row of the declaration-crossing table.
#[derive(Debug, Clone, Copy)]
pub struct DeclRow {
    /// The capability the row describes.
    pub cap: DeclCap,
    /// The C-ABI optional symbol (`b"…\0"`) for `char*`/`int` probing
    /// loaders, or `None` when the capability rides the frozen vtable (or
    /// has no C symbol yet). These MUST be the exact byte constants the
    /// `abi_layout.rs` golden pins — the nursery test asserts row ==
    /// constant, so a rename here fails the test instead of breaking a load.
    pub c_abi_symbol: Option<&'static [u8]>,
    /// The `manifest.json` key the Lua runtime spells the declaration with,
    /// or `None` for a C-only/vtable-native capability with no manifest key.
    pub lua_manifest_key: Option<&'static str>,
    /// The `bk_*` entry point, only for [`DeclCap::OnKline`]-class runtime
    /// hooks. `None` for load-time declarations.
    pub lua_entry_point: Option<LuaEntryPoint>,
    /// The `EngineStrategy` trait method the declaration lands on after
    /// adoption (readability in load receipts; not introspected at runtime).
    pub trait_method: &'static str,
}

const DECLARATION_ROWS: [DeclRow; 11] = [
    // §2.3 modes — symbol present / manifest array; validated by ONE shared
    // §7.4 validator both sides call.
    DeclRow {
        cap: DeclCap::Modes,
        c_abi_symbol: Some(super::BK_DECLARE_MODES_SYMBOL),
        lua_manifest_key: Some("modes"),
        lua_entry_point: None,
        trait_method: "declare_modes",
    },
    // E2-b exemptions — JSON payload both channels spell with the SAME keys
    // (the table above).
    DeclRow {
        cap: DeclCap::GateExemptions,
        c_abi_symbol: Some(super::BK_GATE_EXEMPTIONS_SYMBOL),
        lua_manifest_key: Some("gate_exemptions"),
        lua_entry_point: None,
        trait_method: "gate_exemptions",
    },
    // E2-c evolvable knobs — C symbol payload vs the manifest `tunables`
    // min/max domains (the kernel converts BOTH into `KnobSpec`s).
    DeclRow {
        cap: DeclCap::EvolvableKnobs,
        c_abi_symbol: Some(super::BK_EVOLVABLE_KNOBS_SYMBOL),
        lua_manifest_key: Some("tunables"),
        lua_entry_point: None,
        trait_method: "evolvable_knobs",
    },
    // E-parity bind — C-only. The Lua side receives the same verdict through
    // `bk.book(...).fresh` (the eval-books push), so there is no manifest key.
    DeclRow {
        cap: DeclCap::BindEvalCtx,
        c_abi_symbol: Some(super::BK_BIND_EVAL_CTX_SYMBOL),
        lua_manifest_key: None,
        lua_entry_point: None,
        trait_method: "on_eval_books",
    },
    DeclRow {
        cap: DeclCap::ConfigView,
        c_abi_symbol: Some(super::BK_CONFIG_VIEW_SYMBOL),
        lua_manifest_key: None,
        lua_entry_point: None,
        trait_method: "config_view_json",
    },
    // #642's four optional symbols ↔ manifest keys.
    DeclRow {
        cap: DeclCap::HoldsToSettlement,
        c_abi_symbol: Some(super::BK_SETTLEMENT_HOLDS_SYMBOL),
        lua_manifest_key: Some("holds_to_settlement"),
        lua_entry_point: None,
        trait_method: "holds_to_settlement",
    },
    DeclRow {
        cap: DeclCap::OnKline,
        c_abi_symbol: Some(super::BK_ON_KLINE_SYMBOL),
        lua_manifest_key: None,
        lua_entry_point: Some(LuaEntryPoint::OnKline),
        trait_method: "on_kline",
    },
    DeclRow {
        cap: DeclCap::EntryPair,
        c_abi_symbol: Some(super::BK_ENTRY_PAIR_SYMBOL),
        lua_manifest_key: None,
        lua_entry_point: None,
        trait_method: "entry_pair",
    },
    DeclRow {
        cap: DeclCap::TrailingExit,
        c_abi_symbol: Some(super::BK_TRAILING_EXIT_SYMBOL),
        lua_manifest_key: Some("trailing_exit"),
        lua_entry_point: None,
        trait_method: "wants_trailing_exit",
    },
    DeclRow {
        cap: DeclCap::AtomicBatch,
        c_abi_symbol: Some(super::BK_ATOMIC_BATCH_SYMBOL),
        lua_manifest_key: Some("atomic_batch"),
        lua_entry_point: None,
        trait_method: "atomic_batch",
    },
    // Entry-fill doctrine: C row `None` (the posture reaches the kernel
    // through the intent JSON the kernel already adopts — no own symbol on
    // ABI 2), manifest key `entry_maker`.
    DeclRow {
        cap: DeclCap::EntryMaker,
        c_abi_symbol: None,
        lua_manifest_key: Some("entry_maker"),
        lua_entry_point: None,
        trait_method: "entry_maker",
    },
];

/// The ONE declaration-crossing table (see module docs). Order = model v1's
/// row order; appending (never mutating) is the only evolution.
pub const DECLARATION_MAP: &[DeclRow] = &DECLARATION_ROWS;

impl DeclCap {
    /// The row of `self` in [`DECLARATION_MAP`]. Every variant has a row —
    /// the table is total, and the nursery test pins the totality.
    pub fn row(self) -> &'static DeclRow {
        DECLARATION_MAP
            .iter()
            .find(|r| r.cap == self)
            .expect("every DeclCap has a DECLARATION_MAP row (totality is test-pinned)")
    }

    /// The capability's load-receipt name (the recipe `strategy.load` and
    /// `strategy.list` rows spell with, snake_case).
    pub fn declaration_name(self) -> &'static str {
        match self {
            DeclCap::Modes => "modes",
            DeclCap::GateExemptions => "gate_exemptions",
            DeclCap::EvolvableKnobs => "evolvable_knobs",
            DeclCap::BindEvalCtx => "bind_eval_ctx",
            DeclCap::ConfigView => "config_view",
            DeclCap::HoldsToSettlement => "holds_to_settlement",
            DeclCap::OnKline => "on_kline",
            DeclCap::EntryPair => "entry_pair",
            DeclCap::TrailingExit => "trailing_exit",
            DeclCap::AtomicBatch => "atomic_batch",
            DeclCap::EntryMaker => "entry_maker",
        }
    }
}

// ── the Lua entry-point contract (the `bk_evaluate`-family names) ──────────

/// A `bk_*` entry point a Lua strategy MAY define. [`LuaEntryPoint::Evaluate`]
/// is REQUIRED (the loader refuses its absence); the rest are optional and
/// skipped when absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LuaEntryPoint {
    /// `bk_evaluate` — REQUIRED (§6.5).
    Evaluate,
    /// `bk_on_book` — every book/top-of-book update.
    OnBook,
    /// `bk_on_round` — a round transition.
    OnRound,
    /// `bk_on_kline` — CLOSED bars only (§6.6 / §10.4).
    OnKline,
    /// `bk_thesis_death` — the strategy's own "my entry thesis is dead"
    /// predicate, consulted once per held position each evaluation cycle
    /// (#961). Truthy = the thesis is dead; the RUNTIME (never the strategy)
    /// then injects the unconditional full-taker exit intent tagged
    /// `thesis_death_exit` for that position, so a package cannot suppress or
    /// de-scope its own death. A package may instead declare the global
    /// `BK_THESIS_DEATH_ENABLE = true` to run the stdlib default predicate
    /// (`bk.thesis_death_default`) over every holding. Neither declared = the
    /// package declares no death conditions, which
    /// `scripts/thesis-death-check.mjs` reports (mandatory for new packages).
    ThesisDeath,
}

impl LuaEntryPoint {
    /// Every entry point, with the REQUIRED one first (the loader's check
    /// order). Kept as data so a fifth entry point is one variant + one row.
    pub const ALL: [LuaEntryPoint; 5] = [
        LuaEntryPoint::Evaluate,
        LuaEntryPoint::OnBook,
        LuaEntryPoint::OnRound,
        LuaEntryPoint::OnKline,
        LuaEntryPoint::ThesisDeath,
    ];

    /// The global name the sandbox looks the function up under.
    pub const fn global_name(self) -> &'static str {
        match self {
            LuaEntryPoint::Evaluate => "bk_evaluate",
            LuaEntryPoint::OnBook => "bk_on_book",
            LuaEntryPoint::OnRound => "bk_on_round",
            LuaEntryPoint::OnKline => "bk_on_kline",
            LuaEntryPoint::ThesisDeath => "bk_thesis_death",
        }
    }
}

// ── the capability GRANT (scope item 2 — made by the host, immutable) ─────

/// Which projection of host state a granter may hand out. Failure mode is
/// named per variant at the `bk.*` projection site, not here — this is the
/// bit-key list, one entry per projection the sandbox install creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataSurface {
    /// The host clock and round timing (`bk.now_ms` / `bk.round`).
    RoundTiming,
    /// The live round's market rows (`bk.markets`).
    Markets,
    /// Order-book projections with the freshness verdict (`bk.book`).
    Books,
    /// This strategy's hot-parameter bag (`bk.params`).
    Params,
    /// CLOSED bars (`bk.kline`) — empty until a host actually feeds a bar.
    Klines,
    /// THIS strategy's display-only account view (`bk.account`, §11.1: no
    /// credentials, ever).
    AccountDisplay,
    /// THIS strategy's open positions (`bk.holdings`), knife-sliced in the
    /// kernel before the push.
    OwnHoldings,
    /// The one fee schedule (`bk.fees`).
    FeeSchedule,
    /// Confirmed cross-venue mappings (#427, `bk.unified_events`).
    UnifiedEvents,
    /// THIS instance's pair-book views (RFC 568 阶段一, `bk.arb.pairs`;
    /// owner-filtered kernel-side).
    PairViews,
    /// The depth cost model (`bk.vwap_for_size`) over the visible books.
    VwapCostModel,
}

impl DataSurface {
    /// Every surface, in bit-table order (`VISIBILITY_NAMES` agrees with
    /// this).
    pub const ALL: [DataSurface; 11] = [
        DataSurface::RoundTiming,
        DataSurface::Markets,
        DataSurface::Books,
        DataSurface::Params,
        DataSurface::Klines,
        DataSurface::AccountDisplay,
        DataSurface::OwnHoldings,
        DataSurface::FeeSchedule,
        DataSurface::UnifiedEvents,
        DataSurface::PairViews,
        DataSurface::VwapCostModel,
    ];

    /// The machine-readable name (load receipts, `strategy.list` capability
    /// rows and audit lines use ONE vocabulary for a surface).
    pub fn name(self) -> &'static str {
        match self {
            DataSurface::RoundTiming => "round_timing",
            DataSurface::Markets => "markets",
            DataSurface::Books => "books",
            DataSurface::Params => "params",
            DataSurface::Klines => "klines",
            DataSurface::AccountDisplay => "account_display",
            DataSurface::OwnHoldings => "own_holdings",
            DataSurface::FeeSchedule => "fee_schedule",
            DataSurface::UnifiedEvents => "unified_events",
            DataSurface::PairViews => "pair_views",
            DataSurface::VwapCostModel => "vwap_cost_model",
        }
    }
}

/// The `bk.*` visibility the shipped kernels grant every registered strategy
/// (the current posture, stated as data so a new projection has to DECIDE:
/// it either joins this list or it is not granted by default). See each
/// surface's docs for what the projection itself guarantees.
pub const DEFAULT_GRANTED_SURFACES: [DataSurface; 11] = DataSurface::ALL;

/// The host-created, immutable capability grant (issue #843 scope item 2).
///
/// The grant is the explicit statement of WHERE a strategy instance may see
/// (account / venues / tokens / round period) and WHAT it may read. The
/// host — never the strategy — creates it at registration and freezes it
/// (`host_grant` is the only public constructor); every accessor is plain
/// data, nothing derives serde (the grant is in-process host state, not
/// wire), and the DEFAULTS are fail-closed: a host that builds via
/// [`CapabilityGrantBuilder::new`] and forgets to grant sees nothing.
///
/// The shipped posture is called by [`CapabilityGrantBuilder::host_grant`]:
/// everything visible today stays visible (the DEFAULT_GRANTED_SURFACES
/// list), scoped to the instance's own account, the round the kernel
/// dispatches, and the tokens of that round's markets. Narrowing to actual
/// per-request vetting is the NEXT wave's adapter work; what this P1 closes
/// is the model: the grant is a type the host holds, the reject/deny
/// behaviour is expressible, and negative tests can drive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyCapabilityGrant {
    /// The account the instance trades for (the registration account).
    account_id: String,
    /// Venues the instance may observe rows from (empty = default account
    /// scope, no restriction enforced yet by the projections; a non-empty
    /// list is the explicit "these venues only" grant).
    venues: Vec<String>,
    /// Tokens the instance may receive books/holdings for (the round's
    /// market tokens at dispatch; empty in v1 = "not token-filtered yet",
    /// the shipped posture — stated honestly rather than as a fake deny).
    tokens: Vec<String>,
    /// Min `time_left_sec` the period grant covers (`None` = not enforced).
    period_min_time_left_sec: Option<i64>,
    /// Max `time_left_sec` the period grant covers (`None` = not enforced).
    period_max_time_left_sec: Option<i64>,
    /// The granted `bk.*` surfaces (bit table over [`DataSurface::ALL`]).
    granted: u64,
}

impl StrategyCapabilityGrant {
    /// Fail-closed empty grant: no account, no venues, no tokens, no
    /// surfaces. Internal — hosts reach it through the builder.
    const fn empty() -> Self {
        Self {
            account_id: String::new(),
            venues: Vec::new(),
            tokens: Vec::new(),
            period_min_time_left_sec: None,
            period_max_time_left_sec: None,
            granted: 0,
        }
    }

    /// The account the instance trades for.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// The granted venues. Empty in v1 = the default account scope (no
    /// venue restriction is enforced by the projections yet — the shipped
    /// posture, stated rather than faked).
    pub fn venues(&self) -> &[String] {
        &self.venues
    }

    /// The granted tokens. Empty in v1 = "not token-filtered yet".
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// The period floor (`None` = not enforced).
    pub fn period_min_time_left_sec(&self) -> Option<i64> {
        self.period_min_time_left_sec
    }

    /// The period ceiling (`None` = not enforced).
    pub fn period_max_time_left_sec(&self) -> Option<i64> {
        self.period_max_time_left_sec
    }

    /// Is this data surface granted?
    pub fn is_granted(&self, surface: DataSurface) -> bool {
        let bit = surface_bit(surface);
        self.granted & bit == bit
    }

    /// The granted surface names, in [`DataSurface::ALL`]-order (audit /
    /// receipt rendering).
    pub fn granted_surfaces(&self) -> Vec<&'static str> {
        DataSurface::ALL
            .into_iter()
            .filter(|s| self.is_granted(*s))
            .map(|s| s.name())
            .collect()
    }
}

impl std::fmt::Display for StrategyCapabilityGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "account={}",
            if self.account_id.is_empty() {
                "<none>"
            } else {
                &self.account_id
            }
        )?;
        if !self.venues.is_empty() {
            write!(f, " venues={:?}", self.venues)?;
        }
        if !self.tokens.is_empty() {
            write!(f, " tokens={:?}", self.tokens)?;
        }
        if let Some(min) = self.period_min_time_left_sec {
            write!(f, " period_min_time_left_sec={min}")?;
        }
        if let Some(max) = self.period_max_time_left_sec {
            write!(f, " period_max_time_left_sec={max}")?;
        }
        write!(f, " surfaces={}", self.granted_surfaces().join(","))
    }
}

fn surface_bit(surface: DataSurface) -> u64 {
    let index = DataSurface::ALL
        .into_iter()
        .position(|s| s == surface)
        .expect("surface ill-indexed");
    1u64 << index
}

/// Builder for [`StrategyCapabilityGrant`]. Hosts use
/// [`CapabilityGrantBuilder::host_grant`] for the shipped posture; the raw
/// builder is `pub` so a future adapter can narrow a grant without editing
/// this crate.
#[derive(Debug)]
pub struct CapabilityGrantBuilder {
    grant: StrategyCapabilityGrant,
}

impl Default for CapabilityGrantBuilder {
    fn default() -> Self {
        Self {
            grant: StrategyCapabilityGrant::empty(),
        }
    }
}

impl CapabilityGrantBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn account(mut self, account_id: impl Into<String>) -> Self {
        self.grant.account_id = account_id.into();
        self
    }

    pub fn venues(mut self, venues: Vec<String>) -> Self {
        self.grant.venues = venues;
        self
    }

    pub fn tokens(mut self, tokens: Vec<String>) -> Self {
        self.grant.tokens = tokens;
        self
    }

    pub fn period_bounds(
        mut self,
        min_time_left_sec: Option<i64>,
        max_time_left_sec: Option<i64>,
    ) -> Self {
        self.grant.period_min_time_left_sec = min_time_left_sec;
        self.grant.period_max_time_left_sec = max_time_left_sec;
        self
    }

    pub fn grant(mut self, surface: DataSurface) -> Self {
        self.grant.granted |= surface_bit(surface);
        self
    }

    /// Grant every surface in `DEFAULT_GRANTED_SURFACES` (the shipped
    /// posture).
    pub fn all_default_surfaces(mut self) -> Self {
        for s in DEFAULT_GRANTED_SURFACES {
            self.grant.granted |= surface_bit(s);
        }
        self
    }

    /// Freeze into the immutable grant.
    pub fn build(self) -> StrategyCapabilityGrant {
        self.grant
    }
}

/// The host grant for the SHIPPED posture: the instance's own registration
/// account, no venue/token narrowing yet (v1 carries the fields; the
/// dispatched round's tokens are the de-facto token scope), the default
/// period floor and every default surface granted. Exactly the visibility a
/// registered strategy has today, stated AS a grant.
pub fn host_grant(account_id: impl Into<String>) -> StrategyCapabilityGrant {
    CapabilityGrantBuilder::new()
        .account(account_id)
        .all_default_surfaces()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model version is frozen; a bump is a disclosed PR decision.
    #[test]
    fn model_version_is_frozen() {
        assert_eq!(STRATEGY_CAPABILITY_MODEL_VERSION, 1);
    }

    /// The manifest gate and the declaration side anchor on the same "1.0".
    #[test]
    fn manifest_api_is_the_formal_1_0() {
        assert_eq!(MANIFEST_API_VERSION, "1.0");
    }

    /// The exemption keys are the words every channel already spells.
    #[test]
    fn gate_exemption_keys_are_the_existing_wire() {
        assert_eq!(GateExemption::Timing.key(), "timing");
        assert_eq!(GateExemption::Momentum.key(), "momentum");
        assert_eq!(GATE_TIMING_FLOOR_KEY, "timing_min_time_left_sec");
        assert_eq!(GateExemption::Timing.audit_name(), "timing");
    }

    /// Three-state verdict: the ABI ints decode to the same enum the
    /// manifest booleans do; out-of-range reads as "no declaration"
    /// (fail-closed, never a false Trail).
    #[test]
    fn trailing_verdict_decodes_both_channels() {
        assert_eq!(
            TrailingExitVerdict::from_abi_int(-1),
            TrailingExitVerdict::FollowKernel
        );
        assert_eq!(
            TrailingExitVerdict::from_abi_int(0),
            TrailingExitVerdict::Never
        );
        assert_eq!(
            TrailingExitVerdict::from_abi_int(1),
            TrailingExitVerdict::Trail
        );
        assert_eq!(
            TrailingExitVerdict::from_abi_int(42),
            TrailingExitVerdict::FollowKernel,
            "an out-of-range int is not a declaration"
        );
        assert_eq!(
            TrailingExitVerdict::from_manifest(None),
            TrailingExitVerdict::FollowKernel
        );
        assert_eq!(
            TrailingExitVerdict::from_manifest(Some(true)),
            TrailingExitVerdict::Trail
        );
        assert_eq!(
            TrailingExitVerdict::from_manifest(Some(false)),
            TrailingExitVerdict::Never
        );
        // Round-trips.
        for v in [
            TrailingExitVerdict::FollowKernel,
            TrailingExitVerdict::Never,
            TrailingExitVerdict::Trail,
        ] {
            assert_eq!(TrailingExitVerdict::from_abi_int(v.to_abi_int()), v);
        }
        assert_eq!(TrailingExitVerdict::FollowKernel.to_option_bool(), None);
        assert_eq!(TrailingExitVerdict::Trail.to_option_bool(), Some(true));
        assert_eq!(TrailingExitVerdict::Never.to_option_bool(), Some(false));
    }

    /// The map is total, row spellings are unique, and the C symbol column
    /// is exact where a symbol exists.
    #[test]
    fn declaration_map_is_total_and_well_formed() {
        // Every enum variant has its row.
        for cap in [
            DeclCap::Modes,
            DeclCap::GateExemptions,
            DeclCap::EvolvableKnobs,
            DeclCap::BindEvalCtx,
            DeclCap::ConfigView,
            DeclCap::HoldsToSettlement,
            DeclCap::OnKline,
            DeclCap::EntryPair,
            DeclCap::TrailingExit,
            DeclCap::AtomicBatch,
            DeclCap::EntryMaker,
        ] {
            let row = cap.row();
            assert_eq!(row.cap, cap);
            // C symbol and manifest key never collide across rows.
            if let Some(sym) = row.c_abi_symbol {
                assert!(sym.ends_with(b"\0"), "symbol bytes must be NUL-terminated");
                let same_symbol = DECLARATION_MAP
                    .iter()
                    .filter(|r| r.c_abi_symbol == Some(sym))
                    .count();
                assert_eq!(same_symbol, 1, "duplicate C symbol for {sym:?}");
            }
            if let Some(key) = row.lua_manifest_key {
                let same_key = DECLARATION_MAP
                    .iter()
                    .filter(|r| r.lua_manifest_key == Some(key))
                    .count();
                assert_eq!(same_key, 1, "duplicate manifest key `{key}`");
            }
        }
    }

    /// The ten symbol names in the map ARE the golden-pinned byte constants
    /// (the `abi_layout.rs` counterparts — this is the same freeze at the
    /// map level, so a map edit cannot rename a loadable symbol).
    #[test]
    fn map_symbols_are_the_exported_constants() {
        assert_eq!(
            DeclCap::Modes.row().c_abi_symbol,
            Some(super::super::BK_DECLARE_MODES_SYMBOL)
        );
        assert_eq!(
            DeclCap::GateExemptions.row().c_abi_symbol,
            Some(super::super::BK_GATE_EXEMPTIONS_SYMBOL)
        );
        assert_eq!(
            DeclCap::EvolvableKnobs.row().c_abi_symbol,
            Some(super::super::BK_EVOLVABLE_KNOBS_SYMBOL)
        );
        assert_eq!(
            DeclCap::BindEvalCtx.row().c_abi_symbol,
            Some(super::super::BK_BIND_EVAL_CTX_SYMBOL)
        );
        assert_eq!(
            DeclCap::ConfigView.row().c_abi_symbol,
            Some(super::super::BK_CONFIG_VIEW_SYMBOL)
        );
        assert_eq!(
            DeclCap::HoldsToSettlement.row().c_abi_symbol,
            Some(super::super::BK_SETTLEMENT_HOLDS_SYMBOL)
        );
        assert_eq!(
            DeclCap::OnKline.row().c_abi_symbol,
            Some(super::super::BK_ON_KLINE_SYMBOL)
        );
        assert_eq!(
            DeclCap::EntryPair.row().c_abi_symbol,
            Some(super::super::BK_ENTRY_PAIR_SYMBOL)
        );
        assert_eq!(
            DeclCap::TrailingExit.row().c_abi_symbol,
            Some(super::super::BK_TRAILING_EXIT_SYMBOL)
        );
        assert_eq!(
            DeclCap::AtomicBatch.row().c_abi_symbol,
            Some(super::super::BK_ATOMIC_BATCH_SYMBOL)
        );
        assert_eq!(DeclCap::EntryMaker.row().c_abi_symbol, None);
    }

    /// The manifest keys are the words the shipped packages already spell
    /// (freezing them at the map level so a manifest gate and the map
    /// cannot drift).
    #[test]
    fn map_manifest_keys_are_the_shipped_spellings() {
        assert_eq!(DeclCap::Modes.row().lua_manifest_key, Some("modes"));
        assert_eq!(
            DeclCap::GateExemptions.row().lua_manifest_key,
            Some("gate_exemptions")
        );
        assert_eq!(
            DeclCap::EvolvableKnobs.row().lua_manifest_key,
            Some("tunables")
        );
        assert_eq!(
            DeclCap::HoldsToSettlement.row().lua_manifest_key,
            Some("holds_to_settlement")
        );
        assert_eq!(
            DeclCap::TrailingExit.row().lua_manifest_key,
            Some("trailing_exit")
        );
        assert_eq!(
            DeclCap::AtomicBatch.row().lua_manifest_key,
            Some("atomic_batch")
        );
        assert_eq!(
            DeclCap::EntryMaker.row().lua_manifest_key,
            Some("entry_maker")
        );
        assert_eq!(DeclCap::BindEvalCtx.row().lua_manifest_key, None);
        assert_eq!(DeclCap::ConfigView.row().lua_manifest_key, None);
        assert_eq!(DeclCap::OnKline.row().lua_manifest_key, None);
        assert_eq!(DeclCap::EntryPair.row().lua_manifest_key, None);
    }

    /// The OnKline row crosses to the Lua entry point by construction.
    #[test]
    fn onkline_row_carries_the_lua_entry_point() {
        let row = DeclCap::OnKline.row();
        assert_eq!(row.lua_entry_point, Some(LuaEntryPoint::OnKline));
        assert_eq!(LuaEntryPoint::OnKline.global_name(), "bk_on_kline");
        // The only entry-point row in the map.
        let with_entry = DECLARATION_MAP
            .iter()
            .filter(|r| r.lua_entry_point.is_some())
            .count();
        assert_eq!(with_entry, 1);
    }

    /// Every entry point's global name; the required one is first.
    #[test]
    fn lua_entry_points_spell_the_existing_globals() {
        assert_eq!(LuaEntryPoint::ALL[0], LuaEntryPoint::Evaluate);
        let names: Vec<&str> = LuaEntryPoint::ALL.iter().map(|e| e.global_name()).collect();
        assert_eq!(
            names,
            vec![
                "bk_evaluate",
                "bk_on_book",
                "bk_on_round",
                "bk_on_kline",
                "bk_thesis_death",
            ]
        );
    }

    /// A grant defaults to NOTHING (fail-closed); the host posture grants
    /// exactly the default surface list; the builder narrows honestly; and
    /// the frozen value is immutable in every accessor.
    #[test]
    fn grant_defaults_to_nothing_and_host_grant_is_the_shipped_posture() {
        let bare = CapabilityGrantBuilder::new().build();
        for s in DataSurface::ALL {
            assert!(
                !bare.is_granted(s),
                "{:?} must not be granted by default",
                s
            );
        }
        assert!(bare.granted_surfaces().is_empty());
        assert_eq!(bare.account_id(), "", "no account unless granted");
        assert!(bare.venues().is_empty() && bare.tokens().is_empty());

        let host = host_grant("default");
        assert_eq!(host.account_id(), "default");
        for s in DEFAULT_GRANTED_SURFACES {
            assert!(host.is_granted(s), "host grants {:?} (shipped posture)", s);
        }
        assert_eq!(host.granted_surfaces().len(), DataSurface::ALL.len());
        assert!(host.venues().is_empty(), "v1 carries no venue narrowing");

        // A narrowed grant denies what it did not grant.
        let narrowed = CapabilityGrantBuilder::new()
            .account("acct-2")
            .grant(DataSurface::Books)
            .build();
        assert!(narrowed.is_granted(DataSurface::Books));
        assert!(!narrowed.is_granted(DataSurface::AccountDisplay));
        assert_eq!(
            narrowed.granted_surfaces(),
            vec!["books"],
            "granted rows list honestly"
        );

        // Period bounds ride the grant.
        let bounded = CapabilityGrantBuilder::new()
            .period_bounds(Some(30), Some(300))
            .build();
        assert_eq!(bounded.period_min_time_left_sec(), Some(30));
        assert_eq!(bounded.period_max_time_left_sec(), Some(300));
        assert_eq!(bare.period_min_time_left_sec(), None);
    }

    /// The grant renders its scope for receipts (account first, surfaces
    /// list last) — the audit vocabulary the enable rows will carry.
    #[test]
    fn grant_displays_a_receipt_line() {
        let host = host_grant("default");
        let text = host.to_string();
        assert!(text.starts_with("account=default"), "{text}");
        assert!(text.contains("surfaces="), "{text}");
        assert!(text.contains("books"), "{text}");
        let bare = CapabilityGrantBuilder::new().build();
        assert!(
            bare.to_string().contains("<none>"),
            "empty grant says so instead of an empty account word"
        );
    }
}
