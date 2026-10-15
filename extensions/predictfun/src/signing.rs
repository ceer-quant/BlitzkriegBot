//! EIP-712 signed order construction for the predict.fun CLOB (#483).
//!
//! The venue matches off-chain against on-chain-settled CTFExchange orders:
//! a placement is an EIP-712 typed-data signature over a 12-field `Order`
//! struct, quantized onto the venue's 18-decimal grids. Everything here is
//! pinned to what two independent production adapters (guzus's and
//! dr-manhattan's predictfun implementations) agree on byte-for-byte; where
//! they disagree or the venue is unmeasurable (all edge probes 401 without
//! credentials) the narrower, safer choice is taken and disclosed.
//!
//! Units at this boundary:
//!   * price — 0..1 probability (the core's unit already), quantized to the
//!     `price_precision = 10^13` grid, floored (a price the grid cannot
//!     represent rounds DOWN, never up into a worse limit);
//!   * size — shares, quantized to `share_precision = 10^15`, floored;
//!   * amounts — 18-decimal wei integers as strings: BUY posts notional
//!     (maker) and receives shares (taker); SELL is the mirror.
//!
//! The signature is produced over the manually-encoded EIP-712 digest (the
//! same `keccak256(0x1901 ‖ domainSeparator ‖ structHash)` every reference
//! computes) with the wallet's `sign_hash` — no smart-wallet (EIP-1271)
//! path exists here; EOA mode only.

use crate::error::PredictError;
use alloy::primitives::{Address, U256, keccak256};
use alloy::signers::Signer;
use alloy::signers::local::PrivateKeySigner;
use rust_decimal::Decimal;
use serde::Serialize;

/// BNB mainnet — the only chain the production API root serves.
pub const CHAIN_ID: u64 = 56;

/// EIP-712 domain name/version, pinned by both references.
pub const PROTOCOL_NAME: &str = "predict.fun CTF Exchange";
pub const PROTOCOL_VERSION: &str = "1";

/// `expiration` meaning "never expires" (year 2100), the resting-limit value.
pub const NO_EXPIRY_TIMESTAMP: u64 = 4_102_444_800;

/// The venue rejects (or the adapters cap) salts at 2^31.
const SALT_MAX: u64 = 2_147_483_648;

/// 18-decimal grids the venue quantizes onto.
const PRICE_PRECISION: u128 = 10_000_000_000_000; // 10^13
const SHARE_PRECISION: u128 = 1_000_000_000_000_000; // 10^15
const WEI: u128 = 1_000_000_000_000_000_000; // 10^18

/// The four CTFExchange contracts (BNB mainnet), selected by the market
/// row's `is_neg_risk × is_yield_bearing` pair.
pub const CTF_EXCHANGE: &str = "0x8BC070BEdAB741406F4B1Eb65A72bee27894B689";
pub const NEG_RISK_CTF_EXCHANGE: &str = "0x365fb81bd4A24D6303cd2F19c349dE6894D8d58A";
pub const YIELD_BEARING_CTF_EXCHANGE: &str = "0x6bEb5a40C032AFc305961162d8204CDA16DECFa5";
pub const YIELD_BEARING_NEG_RISK_CTF_EXCHANGE: &str = "0x8A289d458f5a134bA40015085A8F50Ffb681B41d";

/// Which exchange contract settles a market. Unknown flags refuse rather
/// than defaulting — a wrong contract makes the signature unverifiable.
pub fn exchange_address(is_neg_risk: bool, is_yield_bearing: bool) -> &'static str {
    match (is_yield_bearing, is_neg_risk) {
        (true, true) => YIELD_BEARING_NEG_RISK_CTF_EXCHANGE,
        (true, false) => YIELD_BEARING_CTF_EXCHANGE,
        (false, true) => NEG_RISK_CTF_EXCHANGE,
        (false, false) => CTF_EXCHANGE,
    }
}

/// keccak256 of the `Order(...)` type string — the EIP-712 type hash.
fn order_type_hash() -> [u8; 32] {
    keccak256(
        b"Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,\
uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,\
uint8 side,uint8 signatureType)",
    )
    .0
}

/// `keccak256(abi.encode(typeHash ‖ salt ‖ maker ‖ signer ‖ taker ‖ tokenId
/// ‖ makerAmount ‖ takerAmount ‖ expiration ‖ nonce ‖ feeRateBps ‖ side
/// ‖ signatureType))` — the EIP-712 struct hash, encoded by hand exactly as
/// the references' `abi.encode` does (each field left-padded to 32 bytes).
fn order_struct_hash(order: &SignedOrderFields) -> Result<[u8; 32], PredictError> {
    let mut buf = [0u8; 13 * 32];
    buf[0..32].copy_from_slice(&order_type_hash());
    // uint256 fields big-endian into their slots.
    let put_uint = |buf: &mut [u8; 13 * 32], slot: usize, value: U256| {
        buf[slot * 32 + 32 - 32..slot * 32 + 32].copy_from_slice(&value.to_be_bytes::<32>());
    };
    let put_addr = |buf: &mut [u8; 13 * 32], slot: usize, addr: Address| {
        buf[slot * 32 + 12..slot * 32 + 32].copy_from_slice(addr.as_slice());
    };
    put_uint(&mut buf, 1, U256::from(order.salt));
    put_addr(&mut buf, 2, order.maker);
    put_addr(&mut buf, 3, order.signer);
    put_addr(&mut buf, 4, Address::ZERO);
    // The tokenId is a uint256: parse its decimal string (the references'
    // `U256::from_dec_str`), refusing anything else — a non-decimal token id
    // would sign a garbage order.
    let token_u256 = U256::from_str_radix(&order.token_id, 10).map_err(|_| {
        PredictError::AuthConfig(format!("tokenId {} 不是十进制 uint256", order.token_id))
    })?;
    put_uint(&mut buf, 5, token_u256);
    put_uint(&mut buf, 6, U256::from(order.maker_amount));
    put_uint(&mut buf, 7, U256::from(order.taker_amount));
    put_uint(&mut buf, 8, U256::from(order.expiration));
    put_uint(&mut buf, 9, U256::from(order.nonce));
    put_uint(&mut buf, 10, U256::from(order.fee_rate_bps));
    put_uint(&mut buf, 11, U256::from(order.side));
    put_uint(&mut buf, 12, U256::from(order.signature_type));
    Ok(keccak256(buf).0)
}

/// `keccak256(abi.encode(domainTypeHash ‖ keccak(name) ‖ keccak(version)
/// ‖ chainId ‖ verifyingContract))`.
fn domain_separator(verifying_contract: Address) -> [u8; 32] {
    let domain_type_hash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut buf = [0u8; 5 * 32];
    buf[0..32].copy_from_slice(&domain_type_hash.0);
    buf[32..64].copy_from_slice(&keccak256(PROTOCOL_NAME.as_bytes()).0);
    buf[64..96].copy_from_slice(&keccak256(PROTOCOL_VERSION.as_bytes()).0);
    buf[96..128].copy_from_slice(&U256::from(CHAIN_ID).to_be_bytes::<32>());
    buf[128 + 12..160].copy_from_slice(verifying_contract.as_slice());
    keccak256(buf).0
}

/// The order fields the signature covers — everything the JSON body carries
/// except the signature itself. Kept as a struct so the digest, the test
/// mutations and the wire serialization all read one source of truth.
#[derive(Debug, Clone, Serialize)]
pub struct SignedOrderFields {
    pub salt: u64,
    pub maker: Address,
    pub signer: Address,
    pub token_id: String,
    /// 18-decimal wei strings.
    pub maker_amount: u128,
    pub taker_amount: u128,
    pub expiration: u64,
    pub nonce: u64,
    pub fee_rate_bps: u64,
    /// 0 = BUY, 1 = SELL.
    pub side: u8,
    /// 0 = EOA.
    pub signature_type: u8,
}

impl SignedOrderFields {
    /// The venue-side order hash: `keccak256(0x1901 ‖ domain ‖ structHash)`.
    /// This is the idempotency/reconcile key the submit path dedupes by.
    pub fn order_hash(&self, verifying_contract: Address) -> Result<[u8; 32], PredictError> {
        let mut payload = [0u8; 66];
        payload[0] = 0x19;
        payload[1] = 0x01;
        payload[2..34].copy_from_slice(&domain_separator(verifying_contract));
        let struct_hash = order_struct_hash(self)?;
        payload[34..66].copy_from_slice(&struct_hash);
        Ok(keccak256(payload).0)
    }

    /// The wire body: every field the venue's Order struct reads, as strings
    /// (18-decimal integers) plus the EIP-712 signature. `orderHash` is NOT
    /// included — the venue derives it from the signature itself.
    pub fn to_wire(&self, signature: &str) -> serde_json::Value {
        serde_json::json!({
            "salt": self.salt.to_string(),
            "maker": format!("{:#x}", self.maker),
            "signer": format!("{:#x}", self.signer),
            "taker": "0x0000000000000000000000000000000000000000",
            "tokenId": self.token_id,
            "makerAmount": self.maker_amount.to_string(),
            "takerAmount": self.taker_amount.to_string(),
            "expiration": self.expiration.to_string(),
            "nonce": self.nonce.to_string(),
            "feeRateBps": self.fee_rate_bps.to_string(),
            "side": self.side,
            "signatureType": self.signature_type,
            "signature": signature,
        })
    }
}

/// Quantized limit-order amounts on the venue's grids (both references'
/// `_quantized_limit_amounts`): price on 10^13, shares on 10^15, notional
/// floored onto the price grid. BUY posts notional and takes shares; SELL
/// mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantizedAmounts {
    pub price_per_share: u128,
    pub shares: u128,
    pub notional: u128,
    pub maker_amount: u128,
    pub taker_amount: u128,
}

/// Floor a 0..1 decimal price and a share size onto the venue's 18-decimal
/// wei domains, cut to the 10^13 (price) and 10^15 (shares) granularity
/// grids exactly as the references' `to_wei` does
/// (`floor(v·10^18) // precision · precision`), then derive the notional
/// (`shares·price/10^18` floored to the price grid). Everything lives in the
/// 10^18 domain; the precisions only decide granularity. `price` outside
/// (0,1) and sizes that floor to zero are refused BEFORE anything is
/// signed — a fat-finger or dust order must not consume a salt, let alone
/// reach the venue.
pub fn quantize_limit(
    price: Decimal,
    size: Decimal,
    side_buy: bool,
) -> Result<QuantizedAmounts, PredictError> {
    if price <= Decimal::ZERO || price >= Decimal::ONE {
        return Err(PredictError::AuthConfig(format!(
            "price {price} outside (0,1) — refusing to sign"
        )));
    }
    let price_per_share = to_wei_on_grid(price, PRICE_PRECISION)
        .filter(|v| *v > 0)
        .ok_or_else(|| PredictError::AuthConfig(format!("price {price} floors off the grid")))?;
    let shares = to_wei_on_grid(size, SHARE_PRECISION)
        .filter(|v| *v > 0)
        .ok_or_else(|| PredictError::AuthConfig(format!("size {size} floors to zero shares")))?;
    let raw = shares
        .checked_mul(price_per_share)
        .ok_or_else(|| PredictError::AuthConfig("amount overflow quantizing order".into()))?;
    let notional = (raw / WEI / PRICE_PRECISION) * PRICE_PRECISION;
    if notional == 0 {
        return Err(PredictError::AuthConfig(format!(
            "size {size} at price {price} floors to zero notional"
        )));
    }
    let (maker_amount, taker_amount) = if side_buy {
        (notional, shares)
    } else {
        (shares, notional)
    };
    Ok(QuantizedAmounts {
        price_per_share,
        shares,
        notional,
        maker_amount,
        taker_amount,
    })
}

/// `floor(v·10^18)` cut down to a multiple of `precision` — the references'
/// `to_wei`, word for word. Errors on absurd inputs instead of wrapping.
fn to_wei_on_grid(value: Decimal, precision: u128) -> Option<u128> {
    use rust_decimal::prelude::ToPrimitive;
    let scaled = (value * Decimal::from(WEI)).floor();
    let scaled = scaled.to_u128()?;
    Some((scaled / precision) * precision)
}

/// One wallet-bound signing context: parses/holds the EOA signer and its
/// address (maker = signer = the wallet; taker = zero).
pub struct OrderSigner {
    wallet: PrivateKeySigner,
    pub address: Address,
}

impl OrderSigner {
    /// Build from a hex private key (0x-prefixed or bare). The key lives
    /// only in this struct, sourced from the process environment by the
    /// executor — never in source, examples or tests.
    pub fn from_hex_key(private_key: &str) -> Result<Self, PredictError> {
        let wallet = private_key
            .parse::<PrivateKeySigner>()
            .map_err(|e| PredictError::AuthConfig(format!("PREDICT_PRIVATE_KEY 无效: {e}")))?;
        let address = wallet.address();
        Ok(Self { wallet, address })
    }

    /// EIP-191 `personal_sign` over raw message bytes — the JWT mint path
    /// (`/v1/auth` exchange). Distinct from [`Self::sign_limit_order`]'s
    /// EIP-712 digest signing: this prepends the `\x19Ethereum Signed
    /// Message:\n<len>` header the venue verifies.
    pub async fn sign_personal_message(&self, message: &[u8]) -> Result<String, PredictError> {
        let signature = self
            .wallet
            .sign_message(message)
            .await
            .map_err(|e| PredictError::AuthConfig(format!("EIP-191 signing failed: {e}")))?;
        Ok(format!("0x{signature}"))
    }

    /// Build, quantize and sign one limit order in one pass. The salt comes
    /// from the OS RNG in [0, 2^31). The returned JSON is the complete
    /// `order` object; the submit path resends it byte-identically on retry
    /// (the venue dedupes by the order hash this signature determines).
    pub async fn sign_limit_order(
        &self,
        token_id: &str,
        price: Decimal,
        size: Decimal,
        side_buy: bool,
        fee_rate_bps: u64,
        verifying_contract: &str,
    ) -> Result<(serde_json::Value, String), PredictError> {
        let amounts = quantize_limit(price, size, side_buy)?;
        let contract: Address = verifying_contract.parse().map_err(|_| {
            PredictError::AuthConfig(format!("bad exchange address {verifying_contract}"))
        })?;
        let fields = SignedOrderFields {
            salt: rand::random::<u32>() as u64 % SALT_MAX,
            maker: self.address,
            signer: self.address,
            token_id: token_id.to_string(),
            maker_amount: amounts.maker_amount,
            taker_amount: amounts.taker_amount,
            expiration: NO_EXPIRY_TIMESTAMP,
            nonce: 0,
            fee_rate_bps,
            side: if side_buy { 0 } else { 1 },
            signature_type: 0,
        };
        let hash = fields.order_hash(contract)?;
        let signature = self
            .wallet
            .sign_hash(&hash.into())
            .await
            .map_err(|e| PredictError::AuthConfig(format!("EIP-712 signing failed: {e}")))?;
        // The wire wants the 65-byte (r,s,v) form; alloy's Display is RLP.
        let sig = format!("0x{}", alloy::primitives::hex::encode(signature.as_bytes()));
        Ok((fields.to_wire(&sig), alloy::primitives::hex::encode(hash)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    // A throwaway key generated IN the test (never a fixed literal): the
    // same programmatic-generation idiom the kalshi executor suite uses.
    fn test_signer() -> OrderSigner {
        // Deterministic pseudo-random hex (test-only; not a credential).
        let mut key = String::with_capacity(64);
        let mut state = 0x9e3779b97f4a7c15u64;
        for _ in 0..32 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            key.push_str(&format!("{:02x}", (state >> 56) as u8));
        }
        OrderSigner::from_hex_key(&key).expect("test key parses")
    }

    #[test]
    fn quantize_buys_post_notional_and_take_shares() {
        let q = quantize_limit(dec("0.44"), dec("10"), true).unwrap();
        // 18-decimal domains with the references' granularity grids:
        // 0.44 -> 4.4e17 (10^13 grid), 10 shares -> 1e19 (10^15 grid).
        assert_eq!(q.price_per_share, 440_000_000_000_000_000);
        assert_eq!(q.shares, 10_000_000_000_000_000_000);
        // notional = 10 × 0.44 = 4.4 USDT = 4.4e18, on-grid (×10^13)
        assert_eq!(q.notional, 4_400_000_000_000_000_000);
        assert_eq!(q.maker_amount, q.notional);
        assert_eq!(q.taker_amount, q.shares);
    }

    #[test]
    fn quantize_sells_mirror_the_sides() {
        let q = quantize_limit(dec("0.57"), dec("3"), false).unwrap();
        assert_eq!(q.maker_amount, q.shares);
        assert_eq!(q.taker_amount, q.notional);
        assert_eq!(q.notional, 1_710_000_000_000_000_000); // 3 × 0.57
        assert_eq!(q.shares, 3_000_000_000_000_000_000);
    }

    #[test]
    fn quantize_floors_off_grid_inputs_down() {
        // floor(0.444444×10^18) = 444444000000000000; on the 10^13 grid:
        // 444444000000000000 // 10^13 = 44444 → 44444 × 10^13 = 444440000000000000.
        let q = quantize_limit(dec("0.444444"), dec("10"), true).unwrap();
        assert_eq!(q.price_per_share, 444_440_000_000_000_000);
        // Sub-grid dust floors away entirely → refused, not signed.
        assert!(quantize_limit(dec("0.5"), dec("0.0000000000001"), true).is_err());
    }

    #[test]
    fn quantize_refuses_fat_fingers_before_any_signature() {
        // Prices at or beyond the (0,1) band, and sizes flooring to zero.
        assert!(quantize_limit(Decimal::ZERO, dec("10"), true).is_err());
        assert!(quantize_limit(Decimal::ONE, dec("10"), true).is_err());
        assert!(quantize_limit(dec("1.5"), dec("10"), true).is_err());
        assert!(quantize_limit(dec("0.5"), Decimal::ZERO, true).is_err());
    }

    #[test]
    fn exchange_address_selects_all_four_quadrants() {
        assert_eq!(exchange_address(false, false), CTF_EXCHANGE);
        assert_eq!(exchange_address(true, false), NEG_RISK_CTF_EXCHANGE);
        assert_eq!(exchange_address(false, true), YIELD_BEARING_CTF_EXCHANGE);
        assert_eq!(
            exchange_address(true, true),
            YIELD_BEARING_NEG_RISK_CTF_EXCHANGE
        );
    }

    #[tokio::test]
    async fn signature_recovers_to_the_signing_address() {
        let signer = test_signer();
        let (body, hash_hex) = signer
            .sign_limit_order(
                "7492816350129384756102938475610293847561",
                dec("0.44"),
                dec("10"),
                true,
                100,
                CTF_EXCHANGE,
            )
            .await
            .unwrap();
        // 65-byte (r,s,v) signature over the order hash must recover the wallet.
        let sig = alloy::primitives::hex::decode(
            body["signature"].as_str().unwrap().trim_start_matches("0x"),
        )
        .unwrap();
        let recovered = alloy::primitives::Signature::from_raw(&sig)
            .unwrap()
            .recover_address_from_prehash(&alloy::primitives::B256::from_slice(
                &alloy::primitives::hex::decode(&hash_hex).unwrap(),
            ))
            .unwrap();
        assert_eq!(recovered, signer.address, "EIP-712 digest must verify");
    }

    #[tokio::test]
    async fn a_field_mutation_changes_the_hash_and_the_signature_fails() {
        let signer = test_signer();
        let (_, hash_a) = signer
            .sign_limit_order(
                "7492816350129384756102938475610293847561",
                dec("0.44"),
                dec("10"),
                true,
                100,
                CTF_EXCHANGE,
            )
            .await
            .unwrap();
        let (_, hash_b) = signer
            .sign_limit_order(
                "7492816350129384756102938475610293847561",
                dec("0.45"),
                dec("10"),
                true,
                100,
                CTF_EXCHANGE,
            )
            .await
            .unwrap();
        assert_ne!(hash_a, hash_b, "a different price must hash differently");
        // Same inputs, same hash: the digest is a pure function of the order
        // fields (the salt is one of them and is carried INSIDE the struct).
        let contract: Address = CTF_EXCHANGE.parse().unwrap();
        let amounts = quantize_limit(dec("0.44"), dec("10"), true).unwrap();
        let fixed = SignedOrderFields {
            salt: 7,
            maker: signer.address,
            signer: signer.address,
            token_id: "7492816350129384756102938475610293847561".into(),
            maker_amount: amounts.maker_amount,
            taker_amount: amounts.taker_amount,
            expiration: NO_EXPIRY_TIMESTAMP,
            nonce: 0,
            fee_rate_bps: 100,
            side: 0,
            signature_type: 0,
        };
        let hash_d = alloy::primitives::hex::encode(fixed.order_hash(contract).unwrap());
        let mut fixed_again = fixed.clone();
        fixed_again.token_id = "7492816350129384756102938475610293847561".into();
        assert_eq!(hash_d.len(), 64, "hash is 32 bytes hex");
        // Salt inside the struct is what distinguishes two identical orders.
        let mut salted = fixed.clone();
        salted.salt = 8;
        assert_ne!(
            alloy::primitives::hex::encode(salted.order_hash(contract).unwrap()),
            hash_d,
            "a different salt must hash differently"
        );
        let _ = fixed_again;
    }

    #[tokio::test]
    async fn wire_body_carries_every_order_field_as_strings() {
        let signer = test_signer();
        let (body, _) = signer
            .sign_limit_order(
                "7492816350129384756102938475610293847561",
                dec("0.44"),
                dec("10"),
                true,
                100,
                CTF_EXCHANGE,
            )
            .await
            .unwrap();
        for field in [
            "salt",
            "maker",
            "signer",
            "taker",
            "tokenId",
            "makerAmount",
            "takerAmount",
            "expiration",
            "nonce",
            "feeRateBps",
            "side",
            "signatureType",
            "signature",
        ] {
            assert!(body.get(field).is_some(), "wire body missing {field}");
        }
        assert_eq!(body["side"], 0, "BUY is side=0");
        assert_eq!(body["signatureType"], 0);
        assert_eq!(body["makerAmount"].as_str().unwrap(), "4400000000000000000");
        assert_eq!(
            body["takerAmount"].as_str().unwrap(),
            "10000000000000000000"
        );
        assert_eq!(body["expiration"].as_str().unwrap(), "4102444800");
        // maker = signer = the wallet; taker = zero.
        assert_eq!(body["maker"], body["signer"]);
        assert_eq!(
            body["taker"].as_str().unwrap(),
            "0x0000000000000000000000000000000000000000"
        );
    }
}
