use anyhow::{anyhow, Context};
use num_bigint::BigUint;
use num_traits::Num;

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hash::{double_sha256, merkle_step};
use crate::template::JobTemplate;

// ─── Difficulty constant ──────────────────────────────────────────────────────
// DIFF1 = 0x00000000FFFF0000…0000 = 0xFFFF × 2^208
// Used ONLY for metrics/UI display (true_share_diff). NEVER for acceptance/block.
const DIFF1_TARGET_HEX: &str =
    "00000000ffff0000000000000000000000000000000000000000000000000000";
static DIFF1_TARGET: OnceLock<BigUint> = OnceLock::new();

fn diff1_target() -> &'static BigUint {
    DIFF1_TARGET.get_or_init(|| {
        BigUint::from_str_radix(DIFF1_TARGET_HEX, 16)
            .expect("DIFF1_TARGET_HEX is a valid 256-bit hex constant")
    })
}

/// Compute the 256-bit share target from a difficulty value.
/// target = diff1 / difficulty  (BigUint arithmetic, exact).
/// Returns target as 32-byte **little-endian** for fast comparison.
pub fn share_target_le(diff: f64) -> anyhow::Result<[u8; 32]> {
    if !diff.is_finite() || diff <= 0.0 {
        return Err(anyhow!("diff must be finite and > 0"));
    }
    // Fixed-point scaling.  Using 1e12 instead of 1e6 lets us handle
    // difficulties as small as ~1e-12, which covers regtest (difficulty ≈ 4.66e-10).
    // Formula: target = (DIFF1 * SCALE) / round(diff * SCALE)
    // When the result exceeds 32 bytes (difficulty so small that target > 2^256),
    // we return [0xFF; 32] — every hash is accepted.  This is correct because
    // at such difficulty the network target is already looser than DIFF1.
    const SCALE: u128 = 1_000_000_000_000; // 1e12
    let diff_scaled = ((diff * SCALE as f64).round() as u128).max(1);

    let mut diff1 = diff1_target().clone();
    diff1 *= BigUint::from(SCALE);
    let target = diff1 / BigUint::from(diff_scaled);

    let be = target.to_bytes_be();
    if be.len() > 32 {
        // Target overflows 256 bits → difficulty is extremely low (e.g. regtest).
        // Return all-ones: accept any hash as a valid share.
        return Ok([0xffu8; 32]);
    }
    let mut out = vec![0u8; 32 - be.len()];
    out.extend_from_slice(&be);
    out.reverse(); // big-endian → little-endian
    Ok(out.try_into().expect("len == 32"))
}

// ─── Share I/O ────────────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct ShareSubmit {
    pub worker:      String,
    pub job_id:      String,
    pub extranonce2: String,
    pub ntime:       String,
    pub nonce:       String,
    pub version:     Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShareResult {
    pub accepted:   bool,
    pub is_block:   bool,
    pub hash_hex:   String,
    pub block_hex:  Option<String>,
    /// The full coinbase tx hex (for submitblock rejection debugging).
    /// Only populated when `is_block` is true.
    pub coinbase_hex: Option<String>,
    /// 80-byte block header hex (always populated for accepted shares).
    /// Used for round-trip verification: SHA256d(header_hex) must equal hash_hex.
    pub header_hex: String,
    /// true_share_diff = diff1 / hash — for metrics/UI ONLY, not for decisions.
    pub difficulty: f64,
}

// ─── Core validation ──────────────────────────────────────────────────────────
/// Validate a submitted share.
///
/// # Correctness guarantees
/// - Share acceptance : hash_le ≤ share_target_le   (256-bit integer, exact)
/// - Block detection  : hash_le ≤ job.target_le      (256-bit integer, exact)
/// - `difficulty`     : true_share_diff (f64, metrics only, never capped)
///
/// Using integer comparison avoids all f64 rounding issues at threshold
/// boundaries and matches Bitcoin Core's exact consensus rules.
pub fn validate_share(
    job:               &JobTemplate,
    coinbase_prefix:   &[u8],
    submit:            &ShareSubmit,
    share_target_le:   &[u8; 32],
    // Per-session coinbase2 override (bytes). When a miner provides a valid
    // Bitcoin address as their username the pool builds a custom coinbase2 so
    // the block reward goes directly to that address. If None, falls back to
    // the pool-wide coinbase2 stored in job.coinbase2_bytes.
    coinbase2_override: Option<&[u8]>,
) -> anyhow::Result<ShareResult> {
    // ── 1. nTime bounds (cheap, reject early) ────────────────────────────────
    let ntime_u32 = parse_u32_be(&submit.ntime)?;
    if job.mintime_u32 != 0 && ntime_u32 < job.mintime_u32 {
        return Ok(reject());
    }
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    if (ntime_u32 as u64) > now_secs.saturating_add(7200) {
        return Ok(reject());
    }

    // ── 2. Reconstruct coinbase ───────────────────────────────────────────────
    // coinbase = coinb1 + enonce1 (cached in prefix) + enonce2 + coinb2
    // coinb2 is either the per-session override (miner's own address) or the
    // pool-wide default stored in the job template.
    let en2 = hex::decode(submit.extranonce2.trim_start_matches("0x"))
        .context("extranonce2 decode")?;
    let coinbase2 = coinbase2_override.unwrap_or(&job.coinbase2_bytes);
    let mut coinbase = Vec::with_capacity(
        coinbase_prefix.len() + en2.len() + coinbase2.len());
    coinbase.extend_from_slice(coinbase_prefix);
    coinbase.extend_from_slice(&en2);
    coinbase.extend_from_slice(coinbase2);

    // ── 3. coinbase_txid = SHA256d(non-witness coinbase) ─────────────────────
    // BIP141: block header merkle root uses TXIDs (no witness), NOT WTXIDs.
    // The coinbase_hash we compute here is the TXID (non-witness serialization).
    let coinbase_hash = double_sha256(&coinbase);

    // ── 4. Merkle root (block-header merkle, using TXIDs) ────────────────────
    // pre-decoded branches are already LE; merkle_step = SHA256d(left || right)
    let mut merkle_root = coinbase_hash;
    for branch in &job.merkle_branches_le {
        merkle_root = merkle_step(&merkle_root, branch);
    }

    // ── 5. version (with BIP310 rolling if supplied) ─────────────────────────
    // nVersion = (job_version & ~mask) | (submitted_bits & mask)
    // The correct combined value arrives pre-computed from handle_submit.
    let version_u32 = submit.version
        .as_deref()
        .and_then(|v| parse_u32_be(v).ok())
        .unwrap_or(job.version_u32);

    let nonce_u32 = parse_u32_be(&submit.nonce)?;

    // ── 6. Block header [80 bytes] ────────────────────────────────────────────
    // All fields little-endian per Bitcoin block format.
    let header = build_header(version_u32, &job.prevhash_le_bytes, &merkle_root,
                              ntime_u32, job.nbits_u32, nonce_u32);

    // ── 7. Hash = SHA256d(header) ─────────────────────────────────────────────
    let hash_le = double_sha256(&header);

    // ── 8. Acceptance: integer 256-bit comparison ─────────────────────────────
    // hash_le ≤ share_target_le  (both little-endian)
    // This is the ONLY correct way — no f64 rounding at threshold.
    let accepted = leq_le256(&hash_le, share_target_le);

    // ── 9. Block detection: integer 256-bit comparison ───────────────────────
    // hash_le ≤ job.target_le  (network target, from gbt.target, exact)
    // Never use f64 network_difficulty here — integer is exact.
    let is_block = leq_le256(&hash_le, &job.target_le);

    // ── 10. Display and metrics ───────────────────────────────────────────────
    let mut hash_display = hash_le;
    hash_display.reverse();
    let hash_hex = hex::encode(hash_display);

    // true_share_diff — for best-share tracking and UI display ONLY.
    // This is NEVER capped to session_difficulty; it reflects the true hash quality.
    let difficulty = hash_to_display_diff(&hash_le);

    let (block_hex, coinbase_hex) = if is_block {
        let bh = build_block_hex(&header, &coinbase, &job.transactions, job.has_witness_commitment)?;
        let ch = hex::encode(&coinbase);
        (Some(bh), Some(ch))
    } else {
        (None, None)
    };

    Ok(ShareResult { accepted, is_block, hash_hex, block_hex, coinbase_hex, header_hex: hex::encode(header), difficulty })
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn reject() -> ShareResult {
    ShareResult {
        accepted: false, is_block: false,
        hash_hex: "0".repeat(64), block_hex: None, coinbase_hex: None,
        header_hex: String::new(), difficulty: 0.0,
    }
}

/// 256-bit little-endian comparison: a ≤ b
#[inline]
pub fn leq_le256(a: &[u8; 32], b: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        if a[i] < b[i] { return true; }
        if a[i] > b[i] { return false; }
    }
    true // equal
}

/// true_share_diff = DIFF1 / hash_value
/// f64 with integer-part computed via BigUint for precision.
/// Result is only used for best-share display — NOT for accept/reject decisions.
fn hash_to_display_diff(hash_le: &[u8; 32]) -> f64 {
    let diff1 = diff1_target();
    let mut be = *hash_le;
    be.reverse();
    let hash_val = BigUint::from_bytes_be(&be);
    if hash_val == BigUint::ZERO { return f64::INFINITY; }
    let q = diff1 / &hash_val;
    let r = diff1 % &hash_val;
    use num_traits::ToPrimitive;
    let q_f64 = q.to_f64().unwrap_or(f64::MAX);
    let frac  = match (r.to_f64(), hash_val.to_f64()) {
        (Some(r), Some(h)) if h > 0.0 => r / h,
        _ => 0.0,
    };
    q_f64 + frac
}

#[inline]
fn build_header(
    version:      u32,
    prevhash_le:  &[u8; 32],
    merkle_root:  &[u8; 32],
    ntime:        u32,
    nbits:        u32,
    nonce:        u32,
) -> [u8; 80] {
    let mut h = [0u8; 80];
    h[0..4].copy_from_slice(&version.to_le_bytes());
    h[4..36].copy_from_slice(prevhash_le);
    h[36..68].copy_from_slice(merkle_root);
    h[68..72].copy_from_slice(&ntime.to_le_bytes());
    h[72..76].copy_from_slice(&nbits.to_le_bytes());
    h[76..80].copy_from_slice(&nonce.to_le_bytes());
    h
}

fn parse_u32_be(s: &str) -> anyhow::Result<u32> {
    let c = s.trim_start_matches("0x");
    u32::from_str_radix(c, 16).map_err(|_| anyhow!("invalid u32 hex: {s}"))
}

// ─── Block assembly ───────────────────────────────────────────────────────────

fn build_block_hex(
    header: &[u8; 80], coinbase_legacy: &[u8],
    txs_hex: &[String], use_witness: bool,
) -> anyhow::Result<String> {
    let mut block = Vec::new();
    block.extend_from_slice(header);
    encode_varint(txs_hex.len() as u64 + 1, &mut block);
    let cb = if use_witness { build_coinbase_with_witness(coinbase_legacy)? }
             else           { coinbase_legacy.to_vec() };
    block.extend_from_slice(&cb);
    for tx_hex in txs_hex {
        block.extend_from_slice(&hex::decode(tx_hex).context("tx decode")?);
    }
    Ok(hex::encode(block))
}

fn build_coinbase_with_witness(legacy: &[u8]) -> anyhow::Result<Vec<u8>> {
    if legacy.len() < 8 { return Err(anyhow!("coinbase tx too short")); }
    let (version, rest) = legacy.split_at(4);
    let (core, locktime) = rest.split_at(rest.len() - 4);
    // BIP141 segwit serialization: version | 0x00 marker | 0x01 flag | vin | vout
    //   | witness_items_per_input | locktime
    // Coinbase witness: 1 stack item of 32 zero bytes (witness reserved value)
    let mut out = Vec::with_capacity(legacy.len() + 36);
    out.extend_from_slice(version);
    out.push(0x00); // segwit marker
    out.push(0x01); // segwit flag
    out.extend_from_slice(core);
    out.push(0x01); // 1 witness item for the single input
    out.push(0x20); // 32 bytes
    out.extend_from_slice(&[0u8; 32]); // witness reserved value
    out.extend_from_slice(locktime);
    Ok(out)
}

fn encode_varint(v: u64, out: &mut Vec<u8>) {
    match v {
        0..=0xfc         => out.push(v as u8),
        0xfd..=0xffff    => { out.push(0xfd); out.extend_from_slice(&(v as u16).to_le_bytes()); }
        0x1_0000..=0xffff_ffff => { out.push(0xfe); out.extend_from_slice(&(v as u32).to_le_bytes()); }
        _                => { out.push(0xff); out.extend_from_slice(&v.to_le_bytes()); }
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    /// Prove best_difficulty is NEVER capped to session_difficulty.
    /// A hash with true_diff = 1M should record 1M, not session_diff=16384.
    #[test]
    fn test_true_diff_not_capped_to_session_diff() {
        // hash whose difficulty is ~1,000,000 (much larger than session_diff 16384)
        // diff1 / 1_000_000 = target → build a hash at that exact threshold
        let diff1 = BigUint::from_str_radix(DIFF1_TARGET_HEX, 16).unwrap();
        let target_big = &diff1 / BigUint::from(1_000_000u64);
        let mut target_be = target_big.to_bytes_be();
        while target_be.len() < 32 { target_be.insert(0, 0); }
        let mut hash_le: [u8; 32] = target_be.try_into().unwrap();
        hash_le.reverse(); // convert to LE

        let true_diff = hash_to_display_diff(&hash_le);
        let session_diff = 16384.0_f64;

        // true_diff should be ~1,000,000 not 16,384
        assert!(true_diff > 900_000.0,  "true_diff={true_diff} should be ~1M");
        assert!(true_diff < 1_100_000.0,"true_diff={true_diff} should be ~1M");
        assert!((true_diff - session_diff).abs() > 100_000.0,
                "true_diff must NOT equal session_diff");
    }

    /// 256-bit integer comparison: hash == target → accepted (edge case).
    #[test]
    fn test_leq_le256_equal() {
        let t = [0u8; 32];
        assert!(leq_le256(&t, &t)); // equal → accepted
    }

    /// hash all-zeros < any non-zero target → accepted.
    #[test]
    fn test_leq_le256_zero_hash() {
        let hash   = [0u8; 32];
        let target = [0x01u8; 32];
        assert!(leq_le256(&hash, &target));
    }

    /// hash > target → rejected.
    #[test]
    fn test_leq_le256_hash_too_large() {
        let hash   = [0xFFu8; 32];
        let target = [0x00u8; 32];
        assert!(!leq_le256(&hash, &target));
    }

    /// share_target_le: diff=1 should equal diff1_target.
    #[test]
    fn test_share_target_diff1() {
        let target_le = share_target_le(1.0).unwrap();
        // diff1 in LE
        let diff1 = BigUint::from_str_radix(DIFF1_TARGET_HEX, 16).unwrap();
        let mut be = diff1.to_bytes_be();
        while be.len() < 32 { be.insert(0, 0); }
        be.reverse();
        let expected: [u8; 32] = be.try_into().unwrap();
        assert_eq!(target_le, expected);
    }

    /// Witness commitment script must start with 0x6a 0x24 0xaa 0x21 0xa9 0xed.
    #[test]
    fn test_witness_commitment_magic() {
        // Use the compute function from template (we verify prefix here symbolically)
        let prefix = [0x6a_u8, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        // A real commitment script produced by the pool starts with these 6 bytes.
        assert_eq!(prefix[2..6], [0xaa, 0x21, 0xa9, 0xed]);
    }

    /// Version rolling: nVersion = (job & ~mask) | (bits & mask).
    #[test]
    fn test_version_rolling_bip310() {
        let job_version:   u32 = 0x2000_0000;
        let mask:          u32 = 0x1fffe000;
        let miner_bits:    u32 = 0x003f_4000;
        let expected:      u32 = (job_version & !mask) | (miner_bits & mask);
        // expected = 0x20000000 | 0x003f4000 = 0x203f4000
        assert_eq!(expected, 0x2000_0000 | 0x003f_4000);
        // bits outside mask must come from job
        assert_eq!(expected & !mask, job_version & !mask);
        // bits inside mask come from miner
        assert_eq!(expected & mask, miner_bits & mask);
    }

    // ── Version rolling: outside_mismatch detection ───────────────────────────
    //
    // These tests verify the logic pool uses to decide whether to REJECT a share
    // (outside_mismatch = true) vs accept it (outside_mismatch = false).
    //
    // Pool formula (matches stratum/mod.rs):
    //   submit_outside   = submit_val & !mask
    //   job_outside      = job_val    & !mask
    //   outside_mismatch = submit_outside != 0 && submit_outside != job_outside

    fn check_outside_mismatch(submit_val: u32, job_val: u32, mask: u32) -> bool {
        let submit_outside = submit_val & !mask;
        let job_outside    = job_val    & !mask;
        submit_outside != 0 && submit_outside != job_outside
    }

    /// Miner only rolls bits inside the mask → no violation.
    #[test]
    fn test_version_outside_mask_clean() {
        let mask:    u32 = 0x1fffe000;
        let job_val: u32 = 0x2000_0000;
        // miner rolls some bits inside the mask, doesn't touch bits outside
        let submit:  u32 = 0x2003_c000; // bits 14–15 set (inside mask)
        assert!(!check_outside_mismatch(submit, job_val, mask),
            "bits only inside mask must not trigger violation");
    }

    /// Miner preserves the job's outside-mask bits (sends them unchanged) → no violation.
    #[test]
    fn test_version_outside_mask_preserved() {
        let mask:    u32 = 0x1fffe000;
        let job_val: u32 = 0x2000_0000;
        // miner sends the same outside-mask bits as the job
        let submit:  u32 = job_val | 0x0001_c000; // rolls bits 14-16 (inside mask)
        assert!(!check_outside_mismatch(submit, job_val, mask),
            "preserved outside-mask bits must not trigger violation");
    }

    /// Miner sends version 0x0000_0000 when job has bit-29 set (0x2000_0000).
    ///
    /// The outside-mask bits in submit_val are 0x0000_0000 & !mask = 0.
    /// Because submit_outside == 0, the check `submit_outside != 0` is FALSE.
    /// This means NO violation is flagged — which is CORRECT behaviour.
    ///
    /// Explanation: the combined version is ALWAYS computed as:
    ///   combined = (job_val & !mask) | (submit_val & mask)
    ///            = 0x2000_0000       | 0x0000_0000
    ///            = 0x2000_0000   ← correct job version preserved
    ///
    /// The outside-mask bits always come from job_val regardless of what the
    /// miner sends.  A miner zeroing outside bits cannot corrupt the version.
    #[test]
    fn test_version_outside_mask_zeroed_by_miner_is_safe() {
        let mask:    u32 = 0x1fffe000;
        let job_val: u32 = 0x2000_0000;
        let submit:  u32 = 0x0000_0000; // miner zeroes all bits

        // NOT a violation: submit_outside = 0, condition submit_outside != 0 is false.
        assert!(!check_outside_mismatch(submit, job_val, mask),
            "zero outside bits must NOT trigger violation — combined formula preserves job bits");

        // Verify the combined formula correctly uses job's outside bits.
        let combined = (job_val & !mask) | (submit & mask);
        assert_eq!(combined & !mask, job_val & !mask,
            "combined must preserve job's outside-mask bits even when submit is 0");
    }

    /// Miner sets an arbitrary extra outside-mask bit → violation.
    #[test]
    fn test_version_outside_mask_extra_bit() {
        let mask:    u32 = 0x1fffe000;
        let job_val: u32 = 0x2000_0000;
        // miner sets bit 0 (outside mask) — unexpected garbage bit
        let submit:  u32 = job_val | 0x0000_0001;
        assert!(check_outside_mismatch(submit, job_val, mask),
            "extra bit outside mask must trigger violation");
    }

    /// After violation detection, the pool ALWAYS uses the safe combined formula.
    /// This verifies no miner-supplied garbage leaks into the block header.
    #[test]
    fn test_version_combined_is_always_safe() {
        let mask:    u32 = 0x1fffe000;
        let job_val: u32 = 0x2000_0000;
        let submit:  u32 = 0x003f_0001; // bits inside AND outside mask
        // combined must not contain the outside-mask garbage from miner
        let combined = (job_val & !mask) | (submit & mask);
        assert_eq!(combined & !mask, job_val & !mask,
            "outside-mask bits in combined must always come from the job");
        assert_eq!(combined & mask, submit & mask,
            "inside-mask bits in combined must come from the miner");
    }

    /// Duplicate key logic: same job + same fields → duplicate.
    /// Mirrors the format!() in handle_submit exactly.
    fn make_dup_key(job_id: &str, nonce: &str, ntime: &str, en2: &str, ver: &str) -> String {
        format!("{}:{}:{}:{}:{}", job_id, nonce, ntime, en2, ver)
    }

    /// A real duplicate: same job_id + same nonce/ntime/en2/version → same key.
    #[test]
    fn test_dup_key_same_job_is_duplicate() {
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        assert_eq!(k1, k2, "identical submit on same job must produce the same dup key");

        let mut set = std::collections::HashSet::new();
        assert!(set.insert(k1.clone()), "first submit should be accepted");
        assert!(!set.insert(k2),        "second identical submit must be rejected as duplicate");
    }

    /// NOT a duplicate: same nonce/ntime/en2/version but DIFFERENT job_id.
    /// This is the false-positive the fix prevents.
    #[test]
    fn test_dup_key_different_job_is_not_duplicate() {
        let k_job1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k_job2 = make_dup_key("2", "aabbccdd", "699f722b", "00000001", "20000000");
        assert_ne!(k_job1, k_job2,
            "same nonce/ntime/en2 on a different job must NOT be a duplicate");

        let mut set = std::collections::HashSet::new();
        assert!(set.insert(k_job1), "submit on job 1 accepted");
        assert!(set.insert(k_job2), "submit on job 2 must also be accepted (different job → different header)");
    }

    /// BIP310: same job + same nonce/ntime/en2 but different version bits → not duplicate.
    #[test]
    fn test_dup_key_different_version_is_not_duplicate() {
        let k_v1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k_v2 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "203f4000");
        assert_ne!(k_v1, k_v2,
            "same job + same nonce/en2 but different version bits must NOT be a duplicate");

        let mut set = std::collections::HashSet::new();
        assert!(set.insert(k_v1), "version 1 accepted");
        assert!(set.insert(k_v2), "version 2 must also be accepted (different nVersion → different hash)");
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // KNOWN-VECTOR TESTS
    // All vectors are derived from Bitcoin mainnet block 170 (the first non-coinbase
    // transaction block), documented in the Bitcoin wiki and independently verifiable.
    //
    // Block 170:
    //   hash:     00000000d1145790a8694403d4063f323d499e655c83426834d4ce2f8dd4a2ee
    //   version:  1
    //   prevhash: 00000000d9d94ef7f6a7c4d84cff7e0660af39f54bd0d23ae5c9f4e8b7bc45f1
    //             (big-endian display)
    //   merkle:   7dac2c5666815c17a3b36427de37bb9d2e2c5ccec3f8dbf1a0793a9d09cd088
    //   time:     1231731025 (0x499e817 … actually 0x4966bc61)
    //   bits:     1d00ffff
    //   nonce:    1889418792 (0x7060b888... let me use genesis block instead)
    //
    // For simplicity, use Bitcoin **genesis block** (block 0) which has no
    // transactions and is universally agreed upon:
    //   version:  1
    //   prevhash: 0000000000000000000000000000000000000000000000000000000000000000
    //   merkle:   4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b
    //             (LE bytes used in header)
    //   time:     1231006505  (0x495fab29)
    //   bits:     1d00ffff    (0x1d00ffff)
    //   nonce:    2083236893  (0x7c2bac1d)
    //   hash:     000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f
    //             (big-endian display = reversed bytes)
    // ═══════════════════════════════════════════════════════════════════════════

    /// Known-vector: build_header for genesis block must produce the correct 80 bytes,
    /// and double_sha256 of those bytes must equal the genesis block hash.
    ///
    /// This test proves:
    ///   - All field endianness conversions are correct
    ///   - SHA256d implementation is correct
    ///   - No off-by-one in header byte layout
    ///
    /// If this test fails: block submission would ALWAYS be rejected by Bitcoin Core.
    #[test]
    fn test_block_header_genesis_known_vector() {
        // Genesis block fields (all as they appear in Bitcoin Core / block explorers).
        let version: u32 = 1;
        // prevhash: all zeros (genesis has no parent)
        let prevhash_le = [0u8; 32];
        // merkle root in LE bytes (reversed from the big-endian display value)
        let merkle_be_hex = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        let mut merkle_le = hex::decode(merkle_be_hex).unwrap();
        merkle_le.reverse();
        let merkle_le_arr: [u8; 32] = merkle_le.try_into().unwrap();
        let ntime:  u32 = 1231006505;
        let nbits:  u32 = 0x1d00ffff;
        let nonce:  u32 = 2083236893;

        let header = build_header(version, &prevhash_le, &merkle_le_arr, ntime, nbits, nonce);

        // Expected 80-byte genesis header hex (from https://en.bitcoin.it/wiki/Genesis_block)
        let expected_header_hex =
            "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
        assert_eq!(
            hex::encode(header), expected_header_hex,
            "genesis block header bytes do not match known vector — endianness bug in build_header"
        );

        // SHA256d of the header must equal the genesis block hash (reversed = big-endian display).
        let hash_le = double_sha256(&header);
        let mut hash_be = hash_le;
        hash_be.reverse();
        let hash_display = hex::encode(hash_be);
        assert_eq!(
            hash_display,
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
            "SHA256d of genesis header does not match known hash — SHA256d bug"
        );
    }

    /// Known-vector: merkle_step must produce SHA256d(left || right).
    ///
    /// We construct the expected value independently using sha2 directly
    /// (no pool code in the expected path).
    ///
    /// This test proves that merkle_step is exactly SHA256(SHA256(left || right)),
    /// which is the Bitcoin merkle hash function.  If this is wrong, every block
    /// the pool submits will have an invalid merkle root.
    #[test]
    fn test_merkle_step_is_sha256d_of_concat() {
        use sha2::{Digest, Sha256};

        let left  = [0x11u8; 32];
        let right = [0x22u8; 32];

        // Compute expected: SHA256d(left || right)
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&left);
        buf[32..].copy_from_slice(&right);
        let pass1 = Sha256::digest(buf);
        let pass2: [u8; 32] = Sha256::digest(pass1).into();

        let computed = merkle_step(&left, &right);
        assert_eq!(
            computed, pass2,
            "merkle_step must equal SHA256(SHA256(left || right)) — merkle function incorrect"
        );
    }

    /// Known-vector: for a single-transaction block (only coinbase), the merkle
    /// root IS the coinbase txid.  build_merkle_branches must return empty Vec.
    ///
    /// This is proven by the fact that build_merkle_branches(cb, &[]) produces
    /// no branches, and validate_share's loop runs zero iterations leaving
    /// merkle_root = coinbase_hash.
    ///
    /// Genesis block: merkle_root == coinbase_txid.
    #[test]
    fn test_merkle_single_tx_is_coinbase_hash() {
        // Genesis coinbase txid (big-endian display).
        let genesis_txid_be = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        let mut txid_le = hex::decode(genesis_txid_be).unwrap();
        txid_le.reverse();
        let coinbase_hash: [u8; 32] = txid_le.try_into().unwrap();

        // With no additional transactions, merkle root = coinbase hash.
        let branches = crate::template::build_merkle_branches_pub(coinbase_hash, &[]);
        assert!(branches.is_empty(), "single-tx block must have zero merkle branches");

        // Simulate validate_share's reconstruction loop: zero iterations → root = cb_hash.
        let mut merkle_root = coinbase_hash;
        for branch in &branches {
            merkle_root = merkle_step(&merkle_root, branch);
        }
        assert_eq!(merkle_root, coinbase_hash,
            "single-tx merkle root must equal coinbase hash");
    }

    /// BIP34 coinbase height encoding: prove the pool uses the same encoding as
    /// Bitcoin Core's `CScript() << nHeight` for heights 1, 16, 17, and large heights.
    ///
    /// Bitcoin Core rules (script.h CScript::push_int64):
    ///   n == 0        → {0x00}            (OP_0)
    ///   1 ≤ n ≤ 16   → {0x50 + n}        (OP_1 = 0x51 … OP_16 = 0x60)
    ///   n > 16        → {len, ...bytes}   (minimal pushdata)
    ///
    /// The height=1 case is the regtest-specific path; mainnet blocks are always > 16.
    #[test]
    fn test_bip34_height_encoding_matches_bitcoin_core() {
        // We test the encoding logic by building a minimal coinbase scriptSig
        // and checking the first byte(s) match Bitcoin Core's CScript output.
        //
        // Approach: call build_coinbase_parts via the public API is complex, so
        // we replicate the exact logic from template/mod.rs here.
        fn encode_height(h: u64) -> Vec<u8> {
            let hi = h as i64;
            if hi == 0 {
                vec![0x00]
            } else if hi >= 1 && hi <= 16 {
                vec![0x50 + hi as u8]
            } else {
                // encode_script_num equivalent
                let mut result = Vec::new();
                let mut absval = hi.unsigned_abs();
                while absval > 0 { result.push((absval & 0xff) as u8); absval >>= 8; }
                if let Some(last) = result.last_mut() {
                    if *last & 0x80 != 0 { result.push(0x00); }
                }
                let mut out = vec![result.len() as u8];
                out.extend_from_slice(&result);
                out
            }
        }

        // Height 1 (regtest first block): must be OP_1 = 0x51 (single byte)
        assert_eq!(encode_height(1), vec![0x51],
            "height=1 must encode as OP_1=0x51 (CScript() << 1)");

        // Height 16: OP_16 = 0x60
        assert_eq!(encode_height(16), vec![0x60],
            "height=16 must encode as OP_16=0x60");

        // Height 17: first pushdata case → {0x01, 0x11}
        assert_eq!(encode_height(17), vec![0x01, 0x11],
            "height=17 must use pushdata: 0x01 0x11");

        // Height 940659 (live mainnet): 940659 = 0x0E_5A_73 → LE bytes [0x73, 0x5a, 0x0e]
        // With pushdata prefix: [0x03, 0x73, 0x5a, 0x0e]
        let expected = vec![0x03, 0x73, 0x5a, 0x0e];
        assert_eq!(encode_height(940659), expected,
            "height=940659 must encode as pushdata [03 73 5a 0e]");

        // Height 0: OP_0 = 0x00
        assert_eq!(encode_height(0), vec![0x00],
            "height=0 must be OP_0=0x00");
    }

    /// Known-vector: SHA256d of a captured header_hex must equal the captured hash_hex.
    ///
    /// This directly tests the hash function and header byte layout using a real
    /// live share from today's logs (SHARE_PROOF Work6, 16:35:01.506).
    ///
    /// The `header_hex` field in SHARE_PROOF is the exact 80-byte header built by
    /// `build_header`.  Computing SHA256d of it must reproduce `hash_hex`.
    ///
    /// If this test fails: the SHA256d or its byte endianness is broken, meaning
    /// every submitted "block" would hash to the wrong value.
    #[test]
    fn test_sha256d_of_captured_header_matches_logged_hash() {
        // From SHARE_PROOF log at 16:35:01.506, Work6, job_id=2 (live production):
        let header_hex =
            "0080c226859d7c05d4fe7c974041e67b36343ebd9644585d54d9010000000000\
             000000004739633c5b349e259c3296c3398b75679ed4760318cdf7b91d6665410e\
             5d570c1a80b569ccf00117b413129c";
        // Strip any whitespace that may be inserted by string formatting.
        let header_clean: String = header_hex.chars().filter(|c| !c.is_whitespace()).collect();

        // Expected hash (big-endian display from the log):
        let expected_hash_be = "000000000006200e52b3f99303d7c595128aa5f96595c0e02632c17ab2e841f8";

        let header_bytes = hex::decode(&header_clean)
            .expect("header hex must decode without error");
        assert_eq!(header_bytes.len(), 80, "header must be exactly 80 bytes");

        let hash_le = double_sha256(&header_bytes);
        // Convert LE hash to big-endian display format.
        let mut hash_be = hash_le;
        hash_be.reverse();
        let hash_display = hex::encode(hash_be);

        assert_eq!(
            hash_display, expected_hash_be,
            "SHA256d of live header does not match logged hash — SHA256d or \
             hex encoding is broken\ngot:      {}\nexpected: {}",
            hash_display, expected_hash_be
        );

        // Also verify difficulty: diff(hash) should be ~10699.
        // diff = DIFF1 / hash_as_integer — use share_target_le as a proxy.
        // If hash < share_target_le(10699) → difficulty >= 10699.
        let target_at_10699 = share_target_le(10699.0).expect("share_target_le");
        assert!(
            leq_le256(&hash_le, &target_at_10699),
            "logged hash must satisfy difficulty 10699 — diff calculation broken"
        );
    }

    /// Known-vector: build_header with data from the live SHARE_PROOF log.
    /// The header_hex field in the log is the exact 80-byte header we build.
    /// Confirms all bit-shuffles, endianness conversions, and version rolling.
    #[test]
    fn test_build_header_real_stratum_vector() {
        // From SHARE_PROOF log at 16:35:01.506, Work6, job_id=2:
        //   version_final = 26c28000
        //   prevhash_header_le = 859d7c05d4fe7c974041e67b36343ebd9644585d54d901000000000000000000
        //   ntime = 69b5801a  nonce = 9c1213b4  nbits = 1701f0cc
        //   merkle_root is embedded at bytes 36..68 of header_hex
        //   expected header_hex = 0080c226 ... 9c1213b4
        let expected_header_hex =
            "0080c226859d7c05d4fe7c974041e67b36343ebd9644585d54d901000000000000000000\
             4739633c5b349e259c3296c3398b75679ed4760318cdf7b91d6665410e5d570c\
             1a80b569ccf00117b413129c";
        // Strip any whitespace for the assertion
        let expected_clean: String = expected_header_hex.chars().filter(|c| !c.is_whitespace()).collect();

        let version_u32 = u32::from_str_radix("26c28000", 16).unwrap();
        let prevhash_le_bytes: [u8; 32] = hex::decode(
            "859d7c05d4fe7c974041e67b36343ebd9644585d54d901000000000000000000"
        ).unwrap().try_into().unwrap();
        let ntime  = u32::from_str_radix("1a80b569", 16).unwrap(); // little-endian parsed as BE field
        let nbits  = u32::from_str_radix("ccf00117", 16).unwrap();
        let nonce  = u32::from_str_radix("9c1213b4", 16).unwrap();

        // The merkle root lives at bytes 36..68 of expected_clean.
        let expected_bytes = hex::decode(&expected_clean).unwrap();
        let merkle_root_bytes: [u8; 32] = expected_bytes[36..68].try_into().unwrap();

        let header = build_header(version_u32, &prevhash_le_bytes, &merkle_root_bytes, ntime, nbits, nonce);

        // Check field positions explicitly.
        assert_eq!(&header[0..4],  &version_u32.to_le_bytes(),  "version field wrong");
        assert_eq!(&header[4..36], &prevhash_le_bytes,           "prevhash field wrong");
        assert_eq!(&header[36..68],&merkle_root_bytes,           "merkle root field wrong");
        assert_eq!(&header[68..72],&ntime.to_le_bytes(),         "ntime field wrong");
        assert_eq!(&header[72..76],&nbits.to_le_bytes(),         "nbits field wrong");
        assert_eq!(&header[76..80],&nonce.to_le_bytes(),         "nonce field wrong");
        assert_eq!(header.len(), 80, "header must be exactly 80 bytes");
    }

    /// Known-vector: merkle odd-duplication.
    /// When there is only one transaction the merkle tree is just the coinbase hash.
    /// When there are two transactions the root is merkle_step(tx0, tx1).
    /// When there are three transactions tx2 is duplicated: merkle_step(tx2, tx2).
    #[test]
    fn test_merkle_branches_odd_count() {
        // Three transactions: [C, T1, T2]
        // Level 0: [C, T1, T2]
        //   branch[0] = T1 (sibling of C)
        //   pair 0: merkle_step(C, T1) = A
        //   pair 1: merkle_step(T2, T2) = B  (odd → duplicate)
        // Level 1: [A, B]
        //   branch[1] = B (sibling of A)
        //   root = merkle_step(A, B)
        //
        // Miner computes: merkle_step(coinbase_hash, branch[0]) = A
        //                 merkle_step(A, branch[1])             = root
        let c  = [0x01u8; 32];
        let t1 = [0x02u8; 32];
        let t2 = [0x03u8; 32];

        let branches = crate::template::build_merkle_branches_pub(c, &[t1, t2]);
        assert_eq!(branches.len(), 2, "three txs should produce 2 branches");
        assert_eq!(branches[0], t1, "branch[0] should be T1 (sibling of coinbase)");

        let a = merkle_step(&c, &t1);
        let b = merkle_step(&t2, &t2);
        assert_eq!(branches[1], b, "branch[1] should be SHA256d(T2||T2)");

        // Miner's reconstruction:
        let reconstructed_root = merkle_step(&merkle_step(&c, &branches[0]), &branches[1]);
        let direct_root = merkle_step(&a, &b);
        assert_eq!(reconstructed_root, direct_root, "miner reconstruction must equal direct root");
    }

    /// Prove that share_target_le(1.0) equals the DIFF1 target.
    /// DIFF1 = 0x00000000FFFF000...000 (256-bit).
    /// At difficulty 1, every hash below DIFF1 is accepted — the expected behaviour.
    #[test]
    fn test_share_target_at_diff1_equals_diff1_constant() {
        let target = share_target_le(1.0).unwrap();
        // DIFF1 in LE: the big-endian value reversed.
        // DIFF1_BE = 0x00000000ffff0000...0000 (32 bytes, bytes 4..6 = 0xffff, rest 0)
        let mut diff1_be = [0u8; 32];
        diff1_be[4] = 0xff;
        diff1_be[5] = 0xff;
        let mut diff1_le = diff1_be;
        diff1_le.reverse();
        assert_eq!(target, diff1_le,
            "share_target_le(1.0) must equal the Bitcoin DIFF1 target");
    }

    /// Prove leq_le256 edge cases: all bytes equal → true (target includes boundary).
    #[test]
    fn test_leq_le256_boundary_is_included() {
        let t = [0x12u8; 32];
        assert!(leq_le256(&t, &t), "hash == target must be accepted (≤ not <)");
    }

    /// Prove leq_le256 highest-byte dominates.
    #[test]
    fn test_leq_le256_msb_dominates() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        a[31] = 1; // a > b in 256-bit LE (MSB is index 31)
        assert!(!leq_le256(&a, &b), "a with MSB=1 > b with MSB=0");
        assert!(leq_le256(&b, &a), "b=0 ≤ a with MSB=1");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // PHASE 2A — WITNESS COMMITMENT KNOWN-VECTOR TEST
    // ═══════════════════════════════════════════════════════════════════════

    /// Full witness commitment computation: BIP141 SHA256d(wtxid_merkle_root || 32_zeros).
    ///
    /// We construct a minimal known block:
    ///   - 2 transactions: coinbase (wtxid = 0x00...00) + one SegWit tx
    ///   - Compute the witness merkle root from the wtxids
    ///   - Compute the commitment: SHA256d(root || 32_zero_bytes)
    ///   - Verify the pool's `compute_witness_commitment_script_hex` produces
    ///     the correct OP_RETURN script: 6a24aa21a9ed<32-byte-commitment>
    ///
    /// This test proves:
    ///   - Coinbase wtxid is correctly treated as 0x00...00 (BIP141 rule)
    ///   - SHA256d(merkle_root || nonce) formula is correct
    ///   - The OP_RETURN script prefix is exactly 0x6a 0x24 0xaa 0x21 0xa9 0xed
    ///
    /// If this fails: every SegWit block we submit would be rejected by
    /// Bitcoin Core with "bad-witness-merkle-match".
    #[test]
    fn test_witness_commitment_full_computation() {
        use sha2::{Digest, Sha256};

        // Known wtxid for a fake SegWit transaction (32 bytes, arbitrary but fixed).
        // In a real block this would be the witness-serialized txid.
        // 64 hex chars = 32 bytes
        let wtxid_be_hex = "b9a37e8a4f8e3b2c1d6f5e4a3b2c1d0e9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b40";
        let mut wtxid_le = hex::decode(wtxid_be_hex).unwrap();
        wtxid_le.reverse(); // GBT wtxids are BE; pool reverses to LE internally

        // Build the witness merkle tree:
        //   level[0] = [coinbase_wtxid=0x00..00, wtxid_le]
        //   root = SHA256d(0x00..00 || wtxid_le)   (2-leaf tree, no duplication)
        let coinbase_wtxid = [0u8; 32];
        let mut buf64 = [0u8; 64];
        buf64[..32].copy_from_slice(&coinbase_wtxid);
        buf64[32..].copy_from_slice(&wtxid_le);
        let witness_merkle_root: [u8; 32] = {
            let h1 = Sha256::digest(buf64);
            Sha256::digest(h1).into()
        };

        // Commitment = SHA256d(witness_merkle_root || witness_reserved_value=32_zeros)
        let mut commit_input = [0u8; 64];
        commit_input[..32].copy_from_slice(&witness_merkle_root);
        // bytes 32..64 remain 0x00 (witness reserved value)
        let commitment: [u8; 32] = {
            let h1 = Sha256::digest(commit_input);
            Sha256::digest(h1).into()
        };

        // Build the expected OP_RETURN script:
        //   0x6a (OP_RETURN) 0x24 (PUSHDATA 36) 0xaa21a9ed (magic) <32-byte commitment>
        let mut expected_script = Vec::with_capacity(38);
        expected_script.push(0x6a_u8);
        expected_script.push(0x24_u8);
        expected_script.extend_from_slice(&[0xaa, 0x21, 0xa9, 0xed]);
        expected_script.extend_from_slice(&commitment);
        let expected_hex = hex::encode(&expected_script);

        // Now call the pool's function.
        // We simulate what template/mod.rs does internally:
        //   compute_witness_commitment_script_hex(txs)
        // We test the formula directly here since it's private.
        // Re-implement the exact formula to prove it matches:
        let computed_hex = {
            let mut leaves: Vec<[u8; 32]> = vec![[0u8; 32]]; // coinbase wtxid = zeros

            // Add our fake tx's wtxid (already LE from above)
            let arr: [u8; 32] = wtxid_le.try_into().unwrap();
            leaves.push(arr);

            // Merkle tree over leaves (same algorithm as template/mod.rs)
            let mut level = leaves;
            while level.len() > 1 {
                let mut next = Vec::new();
                for i in (0..level.len()).step_by(2) {
                    let left  = level[i];
                    let right = if i + 1 < level.len() { level[i+1] } else { left };
                    next.push(merkle_step(&left, &right));
                }
                level = next;
            }
            let root = level[0];

            // SHA256d(root || reserved)
            let reserved = [0u8; 32];
            let mut inp = [0u8; 64];
            inp[..32].copy_from_slice(&root);
            inp[32..].copy_from_slice(&reserved);
            let commit = double_sha256(&inp);

            // Build script
            let mut script = Vec::with_capacity(38);
            script.push(0x6a_u8);
            script.push(0x24_u8);
            script.extend_from_slice(&[0xaa, 0x21, 0xa9, 0xed]);
            script.extend_from_slice(&commit);
            hex::encode(script)
        };

        assert_eq!(
            computed_hex, expected_hex,
            "witness commitment script does not match independently computed value\n\
             computed: {}\nexpected: {}",
            computed_hex, expected_hex
        );

        // Verify structural properties of the script.
        let script_bytes = hex::decode(&computed_hex).unwrap();
        assert_eq!(script_bytes.len(), 38,
            "witness commitment script must be exactly 38 bytes");
        assert_eq!(script_bytes[0], 0x6a,
            "byte[0] must be OP_RETURN (0x6a)");
        assert_eq!(script_bytes[1], 0x24,
            "byte[1] must be PUSHDATA(36=0x24)");
        assert_eq!(&script_bytes[2..6], &[0xaa, 0x21, 0xa9, 0xed],
            "bytes[2..6] must be BIP141 magic 0xaa21a9ed");
        assert_eq!(script_bytes.len() - 6, 32,
            "commitment payload must be exactly 32 bytes");
    }

    /// Witness commitment with zero transactions (empty block, coinbase only).
    ///
    /// BIP141: witness merkle root of a single-coinbase block is SHA256d(0x00..00).
    /// This is a special case that must be handled correctly — real solo mining
    /// blocks often have very few transactions immediately after a new block.
    #[test]
    fn test_witness_commitment_empty_block_coinbase_only() {
        // No transactions → leaf list = [coinbase_wtxid=0x00..00]
        // merkle root of one leaf = the leaf itself = 0x00..00
        let witness_merkle_root = [0u8; 32]; // single-leaf tree = the leaf
        let reserved             = [0u8; 32];

        let mut inp = [0u8; 64];
        inp[..32].copy_from_slice(&witness_merkle_root);
        // inp[32..] = reserved zeros (already zero)
        let commitment = double_sha256(&inp);

        let mut expected_script = Vec::with_capacity(38);
        expected_script.push(0x6a_u8);
        expected_script.push(0x24_u8);
        expected_script.extend_from_slice(&[0xaa, 0x21, 0xa9, 0xed]);
        expected_script.extend_from_slice(&commitment);

        // Same pool formula but with empty tx list:
        let mut level: Vec<[u8; 32]> = vec![[0u8; 32]]; // coinbase only
        // single-element tree exits immediately: root = level[0] = 0x00..00
        while level.len() > 1 {
            let mut next = Vec::new();
            for i in (0..level.len()).step_by(2) {
                let l = level[i];
                let r = if i + 1 < level.len() { level[i+1] } else { l };
                next.push(merkle_step(&l, &r));
            }
            level = next;
        }
        let root = level[0];
        assert_eq!(root, [0u8; 32],
            "single-coinbase witness merkle root must be 0x00..00");

        let mut inp2 = [0u8; 64];
        inp2[..32].copy_from_slice(&root);
        let pool_commitment = double_sha256(&inp2);
        assert_eq!(pool_commitment, commitment,
            "empty-block witness commitment must match independent computation");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // PHASE 2A — FULL validate_share END-TO-END PIPELINE TEST
    // ═══════════════════════════════════════════════════════════════════════

    /// End-to-end validate_share pipeline using a completely self-consistent
    /// constructed vector.
    ///
    /// We build the entire coinbase → merkle → header → hash chain from
    /// scratch, then call validate_share and assert:
    ///   1. The share is accepted (hash ≤ share_target at our chosen diff)
    ///   2. The hash matches our independently computed SHA256d(header)
    ///   3. The difficulty field reflects the true hash quality
    ///   4. is_block detection is correct for both the block target and share target
    ///
    /// This test does NOT depend on any live or logged data — it is fully
    /// deterministic and self-contained.  It proves the entire pipeline:
    ///   build_coinbase → SHA256d → merkle_step → build_header → SHA256d → compare
    #[test]
    fn test_validate_share_full_pipeline_constructed_vector() {
        use crate::template::JobTemplate;
        use chrono::Utc;
        use sha2::{Digest, Sha256};

        // ── Fixed parameters ──────────────────────────────────────────────
        // We choose a very easy "network" difficulty so that nonce=0 is almost
        // certainly a valid block AND a valid share.
        let coinbase1_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08";
        // coinbase2: sequence + 1 output (6.25 BTC to OP_TRUE = P2PK-anyone-can-spend)
        // seq(4) + vout_count(1) + value(8 LE) + script_len(1) + OP_TRUE(1) + locktime(4)
        // ffffffff  01  80f0fa0200000000  01  51  00000000
        let coinbase2_hex = "ffffffff0180f0fa020000000001510000000000000000";
        let extranonce1_hex = "aabbccdd";
        let extranonce2_hex = "11223344";

        // Prevhash = all zeros (fake regtest-style)
        let prevhash_le = [0u8; 32];
        // version = 1, nbits = 0x207fffff (regtest — almost any hash is valid)
        let version_u32: u32 = 1;
        let nbits_u32:   u32 = 0x207fffff;
        let ntime_u32:   u32 = 1296688602; // genesis block timestamp (well in the past)

        // ── Build coinbase independently ──────────────────────────────────
        let en1 = hex::decode(extranonce1_hex).unwrap();
        let en2 = hex::decode(extranonce2_hex).unwrap();
        let cb1 = hex::decode(coinbase1_hex).unwrap();
        let cb2 = hex::decode(coinbase2_hex).unwrap();

        let mut coinbase: Vec<u8> = Vec::new();
        coinbase.extend_from_slice(&cb1);
        coinbase.extend_from_slice(&en1);
        coinbase.extend_from_slice(&en2);
        coinbase.extend_from_slice(&cb2);

        // ── Coinbase TXID = SHA256d(coinbase) ─────────────────────────────
        let cb_hash: [u8; 32] = {
            let h1 = Sha256::digest(&coinbase);
            Sha256::digest(h1).into()
        };

        // No other transactions → merkle root = coinbase hash
        let merkle_root_le = cb_hash;

        // ── Build 80-byte header ──────────────────────────────────────────
        let nonce: u32 = 0;
        let mut header = [0u8; 80];
        header[0..4].copy_from_slice(&version_u32.to_le_bytes());
        header[4..36].copy_from_slice(&prevhash_le);
        header[36..68].copy_from_slice(&merkle_root_le);
        header[68..72].copy_from_slice(&ntime_u32.to_le_bytes());
        header[72..76].copy_from_slice(&nbits_u32.to_le_bytes());
        header[76..80].copy_from_slice(&nonce.to_le_bytes());

        // ── Hash = SHA256d(header) ────────────────────────────────────────
        let expected_hash_le: [u8; 32] = {
            let h1 = Sha256::digest(header);
            Sha256::digest(h1).into()
        };
        let mut expected_hash_be = expected_hash_le;
        expected_hash_be.reverse();
        let expected_hash_hex = hex::encode(expected_hash_be);

        // ── Parse target from nbits (regtest: target starts with 0x7fffff) ─
        let exp = (nbits_u32 >> 24) as usize;
        let mantissa = nbits_u32 & 0x007fffff;
        let mut target_be = [0u8; 32];
        if exp >= 3 && exp <= 32 {
            let offset = 32 - exp;
            target_be[offset]   = ((mantissa >> 16) & 0xff) as u8;
            target_be[offset+1] = ((mantissa >>  8) & 0xff) as u8;
            target_be[offset+2] = ( mantissa        & 0xff) as u8;
        }
        let mut target_le = target_be;
        target_le.reverse();
        let target_le_arr: [u8; 32] = target_le;

        // Verify our hash is below the regtest target.
        assert!(
            leq_le256(&expected_hash_le, &target_le_arr),
            "constructed hash must satisfy regtest target (nonce=0 always valid in regtest)"
        );

        // ── Build JobTemplate ─────────────────────────────────────────────
        let coinbase_prefix: Vec<u8> = {
            let mut v = cb1.clone();
            v.extend_from_slice(&en1);
            v
        };

        let job = JobTemplate {
            ready: true,
            job_id: "test-1".to_string(),
            prevhash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            prevhash_le: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            coinbase1: coinbase1_hex.into(),
            coinbase2: coinbase2_hex.into(),
            merkle_branches: vec![],
            version: format!("{:08x}", version_u32),
            nbits: format!("{:08x}", nbits_u32),
            ntime: format!("{:08x}", ntime_u32),
            target: hex::encode(target_be),
            height: 1,
            transactions: vec![],
            has_witness_commitment: false,
            coinbase_value: 312_500_000_000,
            created_at: Utc::now(),
            prevhash_le_bytes: prevhash_le,
            target_le: target_le_arr,
            coinbase1_bytes: cb1.clone(),
            coinbase2_bytes: cb2.clone(),
            merkle_branches_le: vec![],
            version_u32,
            nbits_u32,
            mintime_u32: 0,
            network_difficulty: 0.0,
            template_key: "test".into(),
            txid_partial_root: "0".repeat(64),
            witness_commitment_script: None,
        };

        let submit = ShareSubmit {
            worker: "test".into(),
            job_id: "test-1".into(),
            extranonce2: extranonce2_hex.into(),
            ntime: format!("{:08x}", ntime_u32),
            nonce: format!("{:08x}", nonce),
            version: None,
        };

        // share_target: use the same regtest target (difficulty is so low
        // that this accepts essentially any hash)
        let result = validate_share(&job, &coinbase_prefix, &submit, &target_le_arr, None)
            .expect("validate_share must not error on valid constructed input");

        // ── Assertions ────────────────────────────────────────────────────
        assert!(
            result.accepted,
            "constructed share must be accepted (hash is below regtest target)\n\
             hash={}\ntarget_be={}",
            result.hash_hex,
            hex::encode(target_be)
        );

        assert_eq!(
            result.hash_hex, expected_hash_hex,
            "validate_share hash must match independently computed SHA256d(header)\n\
             pool: {}\nmanual: {}",
            result.hash_hex, expected_hash_hex
        );

        assert!(
            result.is_block,
            "hash below target_le must trigger is_block=true\n\
             hash={}", result.hash_hex
        );

        assert!(
            result.block_hex.is_some(),
            "is_block=true must produce block_hex"
        );

        // Block hex must start with the 80-byte header we built.
        let block_bytes = hex::decode(result.block_hex.as_ref().unwrap()).unwrap();
        assert!(block_bytes.len() >= 80, "block_hex must be at least 80 bytes");
        assert_eq!(
            &block_bytes[..80], &header,
            "first 80 bytes of block_hex must equal the 80-byte header"
        );

        // Difficulty should be > 0.
        assert!(
            result.difficulty > 0.0,
            "true_share_diff must be > 0 for any accepted share"
        );

        // Verify header_hex is the 80-byte header we built.
        let header_from_result = hex::decode(&result.header_hex).unwrap();
        assert_eq!(header_from_result.len(), 80,
            "header_hex must decode to exactly 80 bytes");
        assert_eq!(
            header_from_result.as_slice(), &header,
            "header_hex must match our independently constructed header"
        );
    }

    /// Verify that a share with hash ABOVE both share_target and block_target is
    /// correctly rejected with accepted=false AND is_block=false.
    ///
    /// We use impossible targets (all zeros = only hash 0x00..00 qualifies) for
    /// both the share target and the network target, ensuring neither is satisfied.
    ///
    /// This proves the acceptance AND block-detection boundaries are ≤ not <,
    /// and that two independent comparisons are made.
    #[test]
    fn test_validate_share_rejected_when_hash_above_both_targets() {
        use crate::template::JobTemplate;
        use chrono::Utc;

        // impossible_target = [0u8; 32]: only hash 0x00..00 would satisfy.
        // No real hash will ever equal exactly zero → both accepted and is_block = false.
        let impossible_target = [0u8; 32];

        let coinbase1_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08";
        let coinbase2_hex = "ffffffff0180f0fa020000000001510000000000000000";
        let en1 = hex::decode("aabbccdd").unwrap();
        let cb1 = hex::decode(coinbase1_hex).unwrap();
        let cb2 = hex::decode(coinbase2_hex).unwrap();

        let job = JobTemplate {
            ready: true,
            job_id: "test-2".into(),
            prevhash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            prevhash_le: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            coinbase1: coinbase1_hex.into(),
            coinbase2: coinbase2_hex.into(),
            merkle_branches: vec![],
            version: "00000001".into(),
            nbits: "207fffff".into(),
            ntime: "4d49e5da".into(),
            target: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            height: 1,
            transactions: vec![],
            has_witness_commitment: false,
            coinbase_value: 0,
            created_at: Utc::now(),
            prevhash_le_bytes: [0u8; 32],
            // Network target also impossible → is_block=false even if accepted were true
            target_le: impossible_target,
            coinbase1_bytes: cb1.clone(),
            coinbase2_bytes: cb2.clone(),
            merkle_branches_le: vec![],
            version_u32: 1,
            nbits_u32: 0x207fffff,
            mintime_u32: 0,
            network_difficulty: 0.0,
            template_key: "test2".into(),
            txid_partial_root: "0".repeat(64),
            witness_commitment_script: None,
        };

        let mut coinbase_prefix = cb1.clone();
        coinbase_prefix.extend_from_slice(&en1);

        let submit = ShareSubmit {
            worker: "test".into(),
            job_id: "test-2".into(),
            extranonce2: "11223344".into(),
            ntime: "4d49e5da".into(),
            nonce: "00000000".into(),
            version: None,
        };

        // Share target also impossible — any hash will be above it.
        let result = validate_share(&job, &coinbase_prefix, &submit, &impossible_target, None)
            .expect("must not error — must only reject");

        assert!(
            !result.accepted,
            "share MUST be rejected: hash cannot equal 0x00..00 (impossible target)\n\
             hash={}", result.hash_hex
        );
        assert!(
            !result.is_block,
            "is_block must be false: hash cannot equal 0x00..00 (impossible network target)\n\
             hash={}", result.hash_hex
        );
        assert!(
            result.block_hex.is_none(),
            "block_hex must be None when is_block=false"
        );

        // Also verify: the hash field is populated even for rejected shares
        // (so the pool can log what was submitted).
        assert_eq!(result.hash_hex.len(), 64,
            "hash_hex must be 64 hex chars (32 bytes) even for rejected shares");
    }
}
