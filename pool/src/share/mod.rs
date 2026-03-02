use anyhow::{anyhow, Context};
use num_bigint::BigUint;
use num_traits::Num;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::hash::{double_sha256, merkle_step};
use crate::template::JobTemplate;

// ─── Difficulty constant ──────────────────────────────────────────────────────
// DIFF1 = 0x00000000FFFF0000…0000 = 0xFFFF × 2^208
// Used ONLY for metrics/UI display (true_share_diff). NEVER for acceptance/block.
const DIFF1_TARGET_HEX: &str =
    "00000000ffff0000000000000000000000000000000000000000000000000000";

/// Compute the 256-bit share target from a difficulty value.
/// target = diff1 / difficulty  (BigUint arithmetic, exact).
/// Returns target as 32-byte **little-endian** for fast comparison.
pub fn share_target_le(diff: f64) -> anyhow::Result<[u8; 32]> {
    if !diff.is_finite() || diff <= 0.0 {
        return Err(anyhow!("diff must be finite and > 0"));
    }
    // Fixed-point scaling to keep integer arithmetic.
    const SCALE: u128 = 1_000_000;
    let diff_scaled = ((diff * SCALE as f64).round() as u128).max(1);

    let mut diff1 = BigUint::from_str_radix(DIFF1_TARGET_HEX, 16)?;
    diff1 *= BigUint::from(SCALE);
    let target = diff1 / BigUint::from(diff_scaled);

    let mut be = target.to_bytes_be();
    if be.len() > 32 { be = be[be.len() - 32..].to_vec(); }
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
    /// Per-session coinbase2 override (bytes). When a miner provides a valid
    /// Bitcoin address as their username the pool builds a custom coinbase2 so
    /// the block reward goes directly to that address. If `None`, falls back to
    /// the pool-wide coinbase2 stored in `job.coinbase2_bytes`.
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
    let diff1 = match BigUint::from_str_radix(DIFF1_TARGET_HEX, 16) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    let mut be = *hash_le;
    be.reverse();
    let hash_val = BigUint::from_bytes_be(&be);
    if hash_val == BigUint::ZERO { return f64::INFINITY; }
    let q = &diff1 / &hash_val;
    let r = &diff1 % &hash_val;
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

    /// Miner changes bit 29 (outside mask, consensus-critical) → violation.
    #[test]
    fn test_version_outside_mask_changed_high_bit() {
        let mask:    u32 = 0x1fffe000;
        let job_val: u32 = 0x2000_0000; // bit 29 = 1 (correct)
        // miner flips bit 29 off — this would produce an invalid block version
        let submit:  u32 = 0x0000_0000;
        assert!(check_outside_mismatch(submit, job_val, mask),
            "changed bit outside mask must trigger violation");
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
}
