//! On-chain redemption of settled positions (issue #175).
//!
//! A position that survives to its market's resolution does not become USDC on
//! its own: the winning conditional tokens have to be burned against the CTF
//! contract before the collateral lands in the wallet. This module is the venue
//! half of that — the core books the settlement and hands over a
//! [`RedemptionRequest`]; this module turns it into a transaction and reports
//! the tx hash + block back.
//!
//! Everything Polymarket-specific lives here: the chain's contract addresses,
//! the binary index sets, the NegRisk adapter branch, the operator-approval the
//! adapter needs and the signer/holder check. The core only ever sees the
//! market-api DTOs.
//!
//! Failure policy: a redemption that did not land is reported as a failure, never
//! as a success — a fabricated tx hash would be worse than a loud error, since
//! the core would then book cash that does not exist. The core owns the alert
//! and the retry schedule; this module only classifies whether a retry could
//! ever help (`manual`).

use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Log;
use alloy::signers::Signer as _;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::Context as _;
use blitzkrieg_market_api::{RedemptionFailure, RedemptionRequest, RedemptionResult};
use polymarket_client_sdk_v2::ctf::Client as CtfClient;
use polymarket_client_sdk_v2::ctf::types::{RedeemNegRiskRequest, RedeemPositionsRequest};
use polymarket_client_sdk_v2::types::{Address, B256, U256};
use polymarket_client_sdk_v2::{ContractConfig, POLYGON, contract_config};
use rust_decimal::Decimal;
use std::str::FromStr;

alloy::sol! {
    #[sol(rpc)]
    interface IERC1155Positions {
        /// The operator approval the NegRisk adapter needs before it can move
        /// the holder's position tokens.
        function isApprovedForAll(address owner, address operator) external view returns (bool);
    }

    /// The collateral transfer a successful redemption emits. Decoded from the
    /// receipt so "the transaction mined" can be told apart from "the wallet
    /// was paid".
    event Transfer(address indexed from, address indexed to, uint256 value);
}

/// Redemption is an extra on-chain subsystem, so it is opt-out by environment:
/// an operator on a shared RPC or one who redeems by hand can switch it off.
pub const REDEEM_ENABLED_VAR: &str = "POLYMARKET_REDEEM_ENABLED";
/// JSON-RPC endpoint used to sign and send the redeem transaction. No default:
/// sending a mainnet transaction through a public endpoint without the
/// operator's knowledge is not something this code does on its own.
pub const RPC_URL_VAR: &str = "POLYGON_RPC_URL";

/// How far the collateral the receipt moved may sit from the payout the ledger
/// booked. One cent: the same tolerance `reconcile.rs` uses against the venue's
/// reported free cash, because the credit the core books has to match the cash
/// the venue actually holds or the accounting audit halts the bot.
pub const PAYOUT_TOLERANCE_USD: Decimal = rust_decimal_macros::dec!(0.01);

/// How a redemption attempt ended, before it becomes a [`RedemptionResult`].
///
/// `ManualRequired` is deliberately distinct from `Failed`: it is a condition no
/// retry can change, and conflating the two would either hammer a hopeless call
/// or hide a fixable one.
#[derive(Debug, Clone, PartialEq)]
pub enum RedeemOutcome {
    Confirmed {
        tx_hash: String,
        block_number: Option<u64>,
        /// Collateral the receipt actually moved to the holder, in USDC.
        paid_usd: Decimal,
    },
    Failed {
        message: String,
        manual: bool,
    },
}

/// The redeem path's configuration, read from the environment once.
#[derive(Debug, Clone)]
pub struct RedeemConfig {
    pub enabled: bool,
    pub rpc_url: Option<String>,
    /// The address that actually holds the conditional tokens (the funder).
    pub holder: Address,
    /// The address that signs the transaction.
    pub signer: Address,
}

impl RedeemConfig {
    /// Read the configuration from the process environment. An error here is
    /// always `manual`: redemption cannot run at all (feature switched off, or
    /// no RPC endpoint), and the caller must report the claim as
    /// manual-required rather than silently leaving it un-redeemed.
    pub fn from_env(signer: Address, holder: Address) -> Result<Self, RedemptionFailure> {
        let enabled = std::env::var(REDEEM_ENABLED_VAR)
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                )
            })
            .unwrap_or(true);
        let rpc_url = std::env::var(RPC_URL_VAR)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if !enabled {
            return Err(manual(format!(
                "{REDEEM_ENABLED_VAR} is off — redeem the settled claim by hand"
            )));
        }
        let Some(rpc_url) = rpc_url else {
            return Err(manual(format!(
                "{RPC_URL_VAR} is not set — no on-chain endpoint to redeem through"
            )));
        };
        Ok(Self {
            enabled,
            rpc_url: Some(rpc_url),
            holder,
            signer,
        })
    }

    /// The check that decides whether a signed transaction from this wallet can
    /// move the position at all.
    ///
    /// CTF `redeemPositions` burns the CALLER's conditional tokens. Polymarket
    /// accounts whose funder is a proxy/deposit wallet hold their positions in
    /// that contract, so an EOA call would mine a transaction that redeems
    /// nothing while looking exactly like success — the one outcome this module
    /// must never produce. Such an account has to redeem through the wallet
    /// contract; this bridge refuses instead of burning gas on a no-op.
    pub fn holder_check(&self) -> Result<(), RedemptionFailure> {
        if self.holder != self.signer {
            return Err(manual(format!(
                "positions are held by {} but the signer is {} — a direct CTF redeem \
                 cannot move them (proxy/deposit-wallet account); redeem manually",
                self.holder, self.signer
            )));
        }
        Ok(())
    }
}

fn manual(message: String) -> RedemptionFailure {
    RedemptionFailure {
        message,
        manual: true,
    }
}

fn failed(failure: RedemptionFailure) -> RedeemOutcome {
    RedeemOutcome::Failed {
        message: failure.message,
        manual: failure.manual,
    }
}

/// The deployed addresses of the venue for one kind of market, straight from the
/// SDK's chain table — never a hand-copied literal. The collateral token in
/// particular is *not* the bridged USDC.e contract a Polygon wallet usually
/// holds, so a hardcoded address there would redeem against the wrong token.
fn chain_config(neg_risk: bool) -> Result<&'static ContractConfig, RedemptionFailure> {
    contract_config(POLYGON, neg_risk).ok_or_else(|| {
        manual(format!(
            "the SDK has no Polymarket contract configuration for chain {POLYGON}"
        ))
    })
}

/// The signer used for redemption, read from the same environment variable the
/// order path uses. Kept separate from `venue.rs`'s signer because the redeem
/// path must be usable (and testable) without an authenticated CLOB client.
pub fn signer_from_env() -> anyhow::Result<PrivateKeySigner> {
    let pk = std::env::var("POLYMARKET_PRIVATE_KEY")
        .context("POLYMARKET_PRIVATE_KEY required for redemption")?;
    let signer = LocalSigner::from_str(pk.trim())?.with_chain_id(Some(POLYGON));
    Ok(signer)
}

/// The funder/holder address from the environment.
pub fn holder_from_env() -> anyhow::Result<Address> {
    let raw = std::env::var("POLYMARKET_FUNDER_ADDRESS")
        .context("POLYMARKET_FUNDER_ADDRESS required for redemption")?;
    Ok(Address::from_str(raw.trim())?)
}

/// Redeem one settled claim on-chain.
///
/// Branches exactly as the venue does: NegRisk markets go through the NegRisk
/// adapter with the share amounts held, ordinary markets through the CTF
/// contract's `redeemPositions` with the binary index sets. Both wait for the
/// receipt, and the receipt is then checked for the collateral transfer itself —
/// a claim is only cash once the wallet was paid, not once a transaction mined.
#[allow(clippy::too_many_lines)] // one linear chain: validate → sign → send → verify
pub async fn redeem_claim(config: &RedeemConfig, request: &RedemptionRequest) -> RedeemOutcome {
    if let Err(f) = config.holder_check() {
        return failed(f);
    }
    let Some(rpc_url) = config.rpc_url.as_deref() else {
        return failed(manual(format!("{RPC_URL_VAR} is not set")));
    };
    let signer = match signer_from_env() {
        Ok(s) => s,
        Err(e) => return failed(manual(format!("redeem signer unavailable: {e}"))),
    };
    let condition_id = match B256::from_str(request.condition_id.trim()) {
        Ok(c) => c,
        Err(e) => {
            return failed(manual(format!(
                "condition id {} is not a bytes32: {e}",
                request.condition_id
            )));
        }
    };
    let chain = match chain_config(request.neg_risk) {
        Ok(c) => c,
        Err(f) => return failed(f),
    };
    // The NegRisk adapter is only configured for the NegRisk chain table; a
    // neg-risk claim on a chain without one could never be redeemed.
    if request.neg_risk && chain.neg_risk_adapter.is_none() {
        return failed(manual(format!(
            "neg-risk claim but the SDK has no NegRisk adapter for chain {POLYGON}"
        )));
    }
    if request.outcome_shares.is_empty() {
        return failed(manual(
            "claim carries no outcome shares to redeem".to_string(),
        ));
    }

    // Send through a wallet-backed provider: the transaction is signed locally
    // and broadcast, and `redeem_positions` awaits the receipt.
    let provider = match ProviderBuilder::new().wallet(signer).connect(rpc_url).await {
        Ok(p) => p,
        Err(e) => {
            // Transport-level: the endpoint is unreachable or refused. Nothing
            // manual about it — retry on the core's backoff.
            return failed(RedemptionFailure {
                message: format!("RPC connect failed: {e}"),
                manual: false,
            });
        }
    };
    let client = match CtfClient::with_neg_risk(provider, POLYGON) {
        Ok(c) => c,
        Err(e) => return failed(manual(format!("CTF client init failed: {e}"))),
    };

    // The adapter moves the holder's ERC1155 position tokens itself, so it needs
    // that holder's operator approval first. Catching it here turns a bare
    // revert into the one instruction an operator can act on.
    if request.neg_risk {
        let adapter = match chain.neg_risk_adapter {
            Some(a) => a,
            None => return failed(manual("NegRisk adapter missing".to_string())),
        };
        let positions = IERC1155Positions::new(chain.conditional_tokens, client.provider().clone());
        match positions
            .isApprovedForAll(config.holder, adapter)
            .call()
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return failed(manual(format!(
                    "the NegRisk adapter {adapter} is not approved to move position tokens held by \
                     {} — call setApprovalForAll({adapter}, true) on {} from that address once, \
                     then this retries on its own",
                    config.holder, chain.conditional_tokens
                )));
            }
            // A view call that cannot run says nothing about approval: retry.
            Err(e) => {
                return failed(RedemptionFailure {
                    message: format!("NegRisk approval check failed: {e}"),
                    manual: false,
                });
            }
        }
    }

    tracing::info!(
        claim = %request.id,
        condition_id = %request.condition_id,
        neg_risk = request.neg_risk,
        expected = %request.expected_payout_usd,
        "redeem: sending on-chain redemption"
    );

    let sent = if request.neg_risk {
        let amounts: Vec<U256> = request
            .outcome_shares
            .iter()
            .map(|s| decimal_to_base_units(*s))
            .collect();
        let req = RedeemNegRiskRequest::builder()
            .condition_id(condition_id)
            .amounts(amounts)
            .build();
        client
            .redeem_neg_risk(&req)
            .await
            .map(|r| (r.transaction_hash, r.block_number))
    } else {
        let req = RedeemPositionsRequest::for_binary_market(chain.collateral, condition_id);
        client
            .redeem_positions(&req)
            .await
            .map(|r| (r.transaction_hash, r.block_number))
    };

    let (tx_hash, block_number) = match sent {
        Ok(v) => v,
        Err(e) => {
            let raw = e.to_string();
            return failed(RedemptionFailure {
                // A rejected call is either a temporary chain problem (gas
                // price, nonce, RPC) or a permanent one (not resolved yet,
                // nothing to redeem). Only the latter stops the retry loop, and
                // only the venue's own revert text can tell them apart.
                manual: looks_permanent(&raw),
                message: classify_message(&raw),
            });
        }
    };

    // "Mined" is not "paid": read the receipt back and count what the collateral
    // actually moved to the holder. A redeem that mines but transfers nothing
    // (a claim already taken, an empty position, a holder that is not the
    // caller) must never be reported as cash.
    let receipt = match client.provider().get_transaction_receipt(tx_hash).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return failed(RedemptionFailure {
                message: format!(
                    "redeem transaction {tx_hash:#x} was sent but its receipt is not available yet \
                     — reporting it unredeemed"
                ),
                manual: false,
            });
        }
        Err(e) => {
            return failed(RedemptionFailure {
                message: format!("receipt fetch failed for {tx_hash:#x}: {e}"),
                manual: false,
            });
        }
    };
    let paid = paid_to_holder(receipt.logs(), chain.collateral, config.holder);
    let expected = request.expected_payout_usd;
    if expected > Decimal::ZERO && paid < expected - PAYOUT_TOLERANCE_USD {
        return failed(manual(format!(
            "redeem transaction {tx_hash:#x} mined in block {block_number:?} but the collateral \
             transfer to {} was {paid} USDC, not the {expected} the settlement booked — the claim \
             is left unredeemed; check it by hand before the cash is assumed",
            config.holder
        )));
    }

    tracing::info!(
        claim = %request.id,
        tx = %format!("{tx_hash:#x}"),
        block = block_number,
        paid = %paid,
        "redeem: confirmed on-chain"
    );
    RedeemOutcome::Confirmed {
        tx_hash: format!("{tx_hash:#x}"),
        block_number: Some(block_number),
        paid_usd: paid,
    }
}

/// Pure: the collateral a mined redeem transaction actually paid `holder`, summed
/// from the receipt's collateral `Transfer` logs.
///
/// A no-op redeem (the transaction mined but the CTF burned nothing of the
/// caller's) sums to zero — exactly the case the caller must never report as
/// success, and the reason this is read from the logs instead of from a wallet
/// balance that the trading loop is also moving.
pub fn paid_to_holder(logs: &[Log], collateral: Address, holder: Address) -> Decimal {
    logs.iter()
        .filter(|l| l.address() == collateral)
        .filter_map(|l| l.log_decode::<Transfer>().ok())
        .filter(|t| t.inner.data.to == holder)
        .fold(Decimal::ZERO, |acc, t| {
            acc + base_units_to_decimal(t.inner.data.value)
        })
}

/// Pure: does this error text describe a condition retrying cannot fix?
/// "condition not resolved", "already redeemed", "nothing to redeem" are
/// terminal; transport/gas/nonce problems are not.
pub fn looks_permanent(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    const TERMINAL: [&str; 7] = [
        "not resolved",
        "not yet resolved",
        "unresolved",
        "already redeemed",
        "payout denominator",
        "invalid condition",
        // The CTF's own revert when the caller holds fewer position tokens than
        // the claim asked to burn: the amounts can only be wrong, not late.
        "insufficient balance",
    ];
    TERMINAL.iter().any(|t| m.contains(t))
}

/// Keep the venue's own words, trimmed, so the operator sees the real revert
/// reason. Never redacted: this string reaches the log and the panel, and an
/// error nobody can read is an error nobody can fix.
fn classify_message(raw: &str) -> String {
    let trimmed = raw.trim();
    let end = trimmed
        .char_indices()
        .take_while(|(i, _)| *i < 400)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(trimmed.len());
    trimmed[..end].to_string()
}

/// Shares (6-decimal fixed point) → the base units the CTF/NegRisk contracts
/// take. Pure and saturating: a negative or absurd value cannot wrap.
pub fn decimal_to_base_units(shares: Decimal) -> U256 {
    if shares <= Decimal::ZERO {
        return U256::ZERO;
    }
    let scaled = shares * Decimal::from(1_000_000u64);
    let s = scaled.trunc().to_string();
    U256::from_str(&s).unwrap_or(U256::ZERO)
}

/// Collateral base units → USDC. Pure; a value too large for `Decimal` reads as
/// zero, which can only ever make a redemption look unpaid (never paid).
pub fn base_units_to_decimal(units: U256) -> Decimal {
    Decimal::from_str(&units.to_string()).unwrap_or_default() / Decimal::from(1_000_000u64)
}

/// Report a `RedeemOutcome` back to the host in the boundary shape.
pub fn to_result(
    request: &RedemptionRequest,
    outcome: RedeemOutcome,
    now_ms: i64,
) -> RedemptionResult {
    match outcome {
        RedeemOutcome::Confirmed {
            tx_hash,
            block_number,
            ..
        } => RedemptionResult {
            id: request.id.clone(),
            condition_id: request.condition_id.clone(),
            tx_hash: Some(tx_hash),
            block_number,
            failure: None,
            at_ms: now_ms,
        },
        RedeemOutcome::Failed { message, manual } => RedemptionResult {
            id: request.id.clone(),
            condition_id: request.condition_id.clone(),
            tx_hash: None,
            block_number: None,
            failure: Some(RedemptionFailure { message, manual }),
            at_ms: now_ms,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolEvent as _;
    use rust_decimal_macros::dec;

    /// A receipt log as the RPC returns it, for an ERC-20 `Transfer`.
    fn transfer_log(collateral: Address, from: Address, to: Address, units: u64) -> Log {
        let event = Transfer {
            from,
            to,
            value: U256::from(units),
        };
        Log {
            inner: alloy::primitives::Log {
                address: collateral,
                data: event.encode_log_data(),
            },
            ..Default::default()
        }
    }

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    /// The collateral and CTF addresses must come from the SDK's chain table.
    /// Regression pin for #175: this module used to hardcode the bridged USDC.e
    /// contract, which is NOT the collateral Polymarket's CTF holds.
    #[test]
    fn addresses_come_from_the_sdk_chain_table() {
        let chain = chain_config(false).unwrap();
        assert_eq!(
            chain.collateral,
            Address::from_str("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB").unwrap()
        );
        assert_ne!(
            chain.collateral,
            Address::from_str("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174").unwrap(),
            "the collateral is not USDC.e"
        );
        assert_eq!(
            chain.conditional_tokens,
            Address::from_str("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045").unwrap()
        );
        // NegRisk configures the same CTF contract plus an adapter.
        let neg = chain_config(true).unwrap();
        assert_eq!(neg.conditional_tokens, chain.conditional_tokens);
        assert_eq!(neg.collateral, chain.collateral);
        assert!(neg.neg_risk_adapter.is_some());
    }

    #[test]
    fn only_collateral_transfers_to_the_holder_count_as_payout() {
        let collateral = addr(0x11);
        let other_token = addr(0x22);
        let holder = addr(0xAA);
        let logs = vec![
            // Someone else's payout on the same market: not ours.
            transfer_log(collateral, addr(0x01), addr(0xBB), 9_000_000),
            // A different token paying us: not the collateral.
            transfer_log(other_token, addr(0x02), holder, 7_000_000),
            // Ours, in two legs (the CTF pays both outcomes the holder kept).
            transfer_log(collateral, addr(0x03), holder, 3_000_000),
            transfer_log(collateral, addr(0x04), holder, 2_000_000),
        ];
        assert_eq!(
            paid_to_holder(&logs, collateral, holder),
            dec!(5),
            "3.000000 + 2.000000 USDC"
        );
    }

    /// The no-op redeem: the transaction mined, the CTF burned nothing of the
    /// caller's, no collateral moved. This must read as zero — the state the
    /// caller reports as unredeemed rather than as cash.
    #[test]
    fn a_mined_but_empty_redeem_pays_nothing() {
        let collateral = addr(0x11);
        let holder = addr(0xAA);
        assert_eq!(paid_to_holder(&[], collateral, holder), Decimal::ZERO);
        // An unrelated event from the collateral (approval, transfer elsewhere).
        let logs = vec![
            transfer_log(collateral, holder, addr(0xBB), 4_000_000),
            transfer_log(collateral, addr(0xCC), addr(0xBB), 1_000_000),
        ];
        assert_eq!(paid_to_holder(&logs, collateral, holder), Decimal::ZERO);
    }

    #[test]
    fn shares_convert_to_six_decimal_base_units() {
        assert_eq!(decimal_to_base_units(dec!(5)), U256::from(5_000_000u64));
        assert_eq!(
            decimal_to_base_units(dec!(0.000001)),
            U256::from(1u64),
            "1 micro-share stays 1 base unit"
        );
        assert_eq!(decimal_to_base_units(dec!(0.0000004)), U256::ZERO);
        assert_eq!(decimal_to_base_units(dec!(-3)), U256::ZERO);
        assert_eq!(base_units_to_decimal(U256::from(5_000_000u64)), dec!(5));
        assert_eq!(base_units_to_decimal(U256::ZERO), Decimal::ZERO);
    }

    #[test]
    fn a_holder_that_is_not_the_signer_is_manual_not_retryable() {
        let config = RedeemConfig {
            enabled: true,
            rpc_url: Some("http://127.0.0.1:1".to_string()),
            holder: addr(0xAA),
            signer: addr(0xBB),
        };
        let failure = config.holder_check().unwrap_err();
        assert!(failure.manual, "a proxy-wallet holder needs a human");
        assert!(
            failure.message.contains("proxy/deposit-wallet"),
            "the message has to name the cause: {}",
            failure.message
        );
        let same = RedeemConfig {
            holder: addr(0xAA),
            signer: addr(0xAA),
            ..config
        };
        assert!(same.holder_check().is_ok());
    }

    #[test]
    fn terminal_reverts_stop_the_retries_and_transport_errors_do_not() {
        assert!(looks_permanent(
            "execution reverted: condition not resolved"
        ));
        assert!(looks_permanent(
            "execution reverted: payout denominator is zero"
        ));
        assert!(looks_permanent(
            "ERC1155: insufficient balance for transfer"
        ));
        assert!(!looks_permanent("nonce too low"));
        assert!(!looks_permanent("connection reset by peer"));
        assert!(!looks_permanent("replacement transaction underpriced"));
        assert!(!looks_permanent("gas required exceeds allowance"));
    }

    #[test]
    fn a_result_maps_a_confirmation_to_a_hash_and_a_failure_to_a_reason() {
        let request = RedemptionRequest {
            id: "cond-1".to_string(),
            condition_id: "cond-1".to_string(),
            neg_risk: false,
            outcome_shares: vec![dec!(5), Decimal::ZERO],
            expected_payout_usd: dec!(5),
            winning_token_id: "tok".to_string(),
        };
        let ok = to_result(
            &request,
            RedeemOutcome::Confirmed {
                tx_hash: "0xabc".to_string(),
                block_number: Some(7),
                paid_usd: dec!(5),
            },
            1_000,
        );
        assert_eq!(ok.tx_hash.as_deref(), Some("0xabc"));
        assert_eq!(ok.block_number, Some(7));
        assert!(ok.failure.is_none());

        let bad = to_result(
            &request,
            RedeemOutcome::Failed {
                message: "RPC connect failed".to_string(),
                manual: false,
            },
            2_000,
        );
        assert!(bad.tx_hash.is_none(), "never a hash without a transfer");
        assert!(!bad.failure.unwrap().manual);
    }
}
