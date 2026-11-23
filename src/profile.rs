//! Lightweight profiling counters for the v4 encoder.
//!
//! Sprint 5.7.31 — added so `nexus profile <file>` can report
//! per-file statistics that tell us WHERE the encoder spends time
//! and what the byte-pattern landscape looks like. The counters
//! here answer questions like:
//!
//!   - How many chain candidates does `peek()` walk on average?
//!   - What fraction of positions emit a match vs a literal?
//!   - What's the distribution of match lengths?
//!   - How often does a hash bucket have multiple entries (chain depth)?
//!
//! ## Cost
//!
//! The counters are `AtomicU64` and only touched on the
//! encode hot path. Each `fetch_add` is a relaxed RMW (~1 ns on
//! modern x86). The histograms are 8 bins × 8 bytes = 64 bytes
//! per histogram; we keep four histograms for a 256-byte footprint.
//!
//! In aggregate this adds <2 % to compress time on the corpus
//! (measured: from 4 649 ms to 4 720 ms across the 9.8 MB
//! `corpus_real/` set, ~1.5 % overhead). The numbers we get
//! back justify the cost because they let us target optimizations
//! to actual data behaviour instead of guessing.
//!
//! ## Reset
//!
//! `ProfileStats::reset()` zeroes everything. `nexus profile`
//! resets before each compress so the report is per-file.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct ProfileStats {
    /// Total `peek()` calls (one per literal + one per lazy probe).
    pub peek_calls: AtomicU64,
    /// Number of `peek()` calls that returned `len >= MIN_MATCH`.
    pub peek_with_match: AtomicU64,
    /// Number of `peek()` calls that returned 0 (no usable match).
    pub peek_no_match: AtomicU64,

    /// Total chain-walk steps summed across all `peek()` calls.
    /// Average = `chain_steps_total / peek_calls`. A high number
    /// means the hash chains are long → many collisions → slower
    /// walks. Optimisation lever: increase MAX_CHAIN_STEPS (more
    /// matches found) or change the hash function.
    pub chain_steps_total: AtomicU64,
    /// Number of peeks where the chain walk hit `MAX_CHAIN_STEPS`
    /// (32) without finding a longer match. Indicates the chain
    /// cap is too low for this content.
    pub chain_steps_capped: AtomicU64,

    /// Histogram of match lengths emitted by `encode()`:
    /// index 0 = no match (literal), 1 = L=3, 2 = L=4..=8,
    /// 3 = L=9..=16, 4 = L=17..=32, 5 = L=33..=64,
    /// 6 = L=65..=128, 7 = L=129..=255.
    pub match_len_hist: [AtomicU64; 8],
    /// Histogram of chain depth at the start of `peek()` (chain
    /// length BEFORE walking). Index = number of items in the
    /// chain at this hash bucket (capped at 7). Tells us how
    /// collision-prone the 3-byte hash is for real data.
    pub chain_depth_hist: [AtomicU64; 8],

    /// Number of LZ77 `Match` ops emitted.
    pub matches_emitted: AtomicU64,
    /// Number of literal ops emitted (bytes that didn't find a match).
    pub literals_emitted: AtomicU64,
    /// Number of dict_ref ops emitted (only on the dict-aware path).
    pub dict_refs_emitted: AtomicU64,

    // ─── Per-phase nanosecond accumulators ─────────────────────────
    // Sprint 5.7.31 — added so we can see WHERE time actually goes
    // across the whole compress call, not just in LZ77. Each phase
    // wraps its hot path with `start = Instant::now(); ... ; add`.
    //
    // The unit is nanoseconds (AtomicU64) — enough precision to time
    // sub-microsecond operations across many calls. Aggregated at the
    // report step.

    /// Total ns spent in CDC chunking.
    pub cdc_ns: AtomicU64,
    /// Total ns spent in dedup lookup + verification.
    pub dedup_ns: AtomicU64,
    /// Total ns spent in classify() + compute_stats().
    pub classify_ns: AtomicU64,
    /// Total ns spent in LZ77 MatchFinder::encode() (all blocks summed).
    pub lz77_ns: AtomicU64,
    /// Total ns spent splitting ops into the 4 rANS streams (per block).
    pub op_split_ns: AtomicU64,
    /// Total ns spent in sparse_rans_encode_u8 (4 streams × N blocks).
    pub sparse_rans_ns: AtomicU64,
    /// Total ns spent in rans_encode itself (within sparse_rans_encode_u8).
    pub rans_encode_ns: AtomicU64,
    /// Total ns spent in dict_codec paths (v4.5 + v4.8 dict lookups).
    pub dict_ns: AtomicU64,
}

impl ProfileStats {
    pub const fn new_const() -> Self {
        Self {
            peek_calls: AtomicU64::new(0),
            peek_with_match: AtomicU64::new(0),
            peek_no_match: AtomicU64::new(0),
            chain_steps_total: AtomicU64::new(0),
            chain_steps_capped: AtomicU64::new(0),
            match_len_hist: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            chain_depth_hist: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            matches_emitted: AtomicU64::new(0),
            literals_emitted: AtomicU64::new(0),
            dict_refs_emitted: AtomicU64::new(0),
            cdc_ns: AtomicU64::new(0),
            dedup_ns: AtomicU64::new(0),
            classify_ns: AtomicU64::new(0),
            lz77_ns: AtomicU64::new(0),
            op_split_ns: AtomicU64::new(0),
            sparse_rans_ns: AtomicU64::new(0),
            rans_encode_ns: AtomicU64::new(0),
            dict_ns: AtomicU64::new(0),
        }
    }

    pub fn reset(&self) {
        self.peek_calls.store(0, Ordering::Relaxed);
        self.peek_with_match.store(0, Ordering::Relaxed);
        self.peek_no_match.store(0, Ordering::Relaxed);
        self.chain_steps_total.store(0, Ordering::Relaxed);
        self.chain_steps_capped.store(0, Ordering::Relaxed);
        for h in &self.match_len_hist {
            h.store(0, Ordering::Relaxed);
        }
        for h in &self.chain_depth_hist {
            h.store(0, Ordering::Relaxed);
        }
        self.matches_emitted.store(0, Ordering::Relaxed);
        self.literals_emitted.store(0, Ordering::Relaxed);
        self.dict_refs_emitted.store(0, Ordering::Relaxed);
        self.cdc_ns.store(0, Ordering::Relaxed);
        self.dedup_ns.store(0, Ordering::Relaxed);
        self.classify_ns.store(0, Ordering::Relaxed);
        self.lz77_ns.store(0, Ordering::Relaxed);
        self.op_split_ns.store(0, Ordering::Relaxed);
        self.sparse_rans_ns.store(0, Ordering::Relaxed);
        self.rans_encode_ns.store(0, Ordering::Relaxed);
        self.dict_ns.store(0, Ordering::Relaxed);
    }

    /// Bin index for a match length. Returns 0 for "no match",
    /// 1 for exactly L=3, etc. L=0 means literal.
    pub fn match_len_bin(len: u32) -> usize {
        match len {
            0 => 0,
            3 => 1,
            4..=8 => 2,
            9..=16 => 3,
            17..=32 => 4,
            33..=64 => 5,
            65..=128 => 6,
            _ => 7,
        }
    }

    /// Bin index for chain depth at the start of `peek()`. Capped at 7.
    pub fn chain_depth_bin(depth: usize) -> usize {
        depth.min(7)
    }
}

/// Global singleton. The CLI resets this before each compress.
/// Always-on, with the small overhead documented in the module
/// header.
pub static PROFILE: ProfileStats = ProfileStats::new_const();

/// Record elapsed nanoseconds since `start` into the given counter.
/// Used by phase-level timing in codec.rs / rans_v4.rs.
#[inline(always)]
pub fn add_ns(counter: &AtomicU64, start: std::time::Instant) {
    let elapsed = start.elapsed().as_nanos() as u64;
    counter.fetch_add(elapsed, Ordering::Relaxed);
}

/// RAII guard that prints the profile report on drop. Used by
/// `nexus profile` so the report always fires even on early exit.
pub struct ProfileGuard {
    enabled: bool,
}

impl ProfileGuard {
    pub fn new() -> Self {
        PROFILE.reset();
        Self { enabled: true }
    }
    pub fn disarm(mut self) {
        self.enabled = false;
    }
}

impl Drop for ProfileGuard {
    fn drop(&mut self) {
        if self.enabled {
            print_report(&PROFILE);
        }
    }
}

/// Pretty-print the profile data to stderr. Format chosen so
/// it's diff-friendly (fixed-width columns) and skimmable for
/// quick scans (header lines + summary at the bottom).
pub fn print_report(p: &ProfileStats) {
    let peek_calls = p.peek_calls.load(Ordering::Relaxed);
    let peek_with = p.peek_with_match.load(Ordering::Relaxed);
    let peek_no = p.peek_no_match.load(Ordering::Relaxed);
    let chain_total = p.chain_steps_total.load(Ordering::Relaxed);
    let chain_capped = p.chain_steps_capped.load(Ordering::Relaxed);
    let matches = p.matches_emitted.load(Ordering::Relaxed);
    let literals = p.literals_emitted.load(Ordering::Relaxed);
    let dict_refs = p.dict_refs_emitted.load(Ordering::Relaxed);

    eprintln!();
    eprintln!("=== engine profile (v4 encode) ===");

    eprintln!();
    eprintln!("peek() calls         : {:>10}", peek_calls);
    eprintln!("  with match (L>=3)  : {:>10}  ({:>5.1} %)", peek_with, pct(peek_with, peek_calls));
    eprintln!("  without match      : {:>10}  ({:>5.1} %)", peek_no, pct(peek_no, peek_calls));

    let avg_chain = if peek_calls == 0 { 0.0 } else { chain_total as f64 / peek_calls as f64 };
    eprintln!();
    eprintln!("chain walk depth     :");
    eprintln!("  total steps        : {:>10}", chain_total);
    eprintln!("  avg per peek       : {:>10.2}", avg_chain);
    eprintln!("  hit MAX_CHAIN cap  : {:>10}  ({:>5.1} % of peeks)",
        chain_capped, pct(chain_capped, peek_calls));

    eprintln!();
    eprintln!("match length histogram:");
    let labels = ["  (literal)", "  L=3", "  L=4..8", "  L=9..16", "  L=17..32", "  L=33..64", "  L=65..128", "  L=129..255"];
    let total_ops = matches + literals + dict_refs;
    for (i, h) in labels.iter().enumerate() {
        let v = p.match_len_hist[i].load(Ordering::Relaxed);
        eprintln!("{}{:>10}  ({:>5.1} %)", h, v, pct(v, total_ops));
    }

    eprintln!();
    eprintln!("chain depth histogram (entries in hash bucket at peek start):");
    for (i, h) in labels.iter().enumerate().take(8) {
        let v = p.chain_depth_hist[i].load(Ordering::Relaxed);
        eprintln!("  depth={}{:>10}  ({:>5.1} %)", i, v, pct(v, peek_calls));
    }

    eprintln!();
    eprintln!("ops emitted          :");
    eprintln!("  literals           : {:>10}  ({:>5.1} %)", literals, pct(literals, total_ops));
    eprintln!("  matches            : {:>10}  ({:>5.1} %)", matches, pct(matches, total_ops));
    eprintln!("  dict_refs          : {:>10}  ({:>5.1} %)", dict_refs, pct(dict_refs, total_ops));
    eprintln!("  total ops          : {:>10}", total_ops);

    if matches > 0 {
        // Avg match length computed from histogram.
        let lens: [u64; 8] = [0, 3, 6, 12, 24, 48, 96, 192];
        let mut total_len: u64 = 0;
        let mut count: u64 = 0;
        for (i, &l) in lens.iter().enumerate() {
            let v = p.match_len_hist[i].load(Ordering::Relaxed);
            total_len += l * v;
            count += v;
        }
        let avg = if count == 0 { 0.0 } else { total_len as f64 / count as f64 };
        eprintln!();
        eprintln!("avg match length     : {:>10.2} bytes (weighted by histogram midpoints)", avg);
    }

    // ─── Per-phase timing breakdown ────────────────────────────────
    let cdc_ns = p.cdc_ns.load(Ordering::Relaxed);
    let dedup_ns = p.dedup_ns.load(Ordering::Relaxed);
    let classify_ns = p.classify_ns.load(Ordering::Relaxed);
    let lz77_ns = p.lz77_ns.load(Ordering::Relaxed);
    let op_split_ns = p.op_split_ns.load(Ordering::Relaxed);
    let sparse_rans_ns = p.sparse_rans_ns.load(Ordering::Relaxed);
    let rans_encode_ns = p.rans_encode_ns.load(Ordering::Relaxed);
    let dict_ns = p.dict_ns.load(Ordering::Relaxed);

    let measured = cdc_ns + dedup_ns + classify_ns + lz77_ns + op_split_ns
        + sparse_rans_ns + rans_encode_ns + dict_ns;
    // The "measured" total excludes header writing, file I/O, the
    // final to-Vec work, and the outer v3-multistream optimisation
    // ("pick smallest of std/rle/dict/local"). It's an UNDER-estimate
    // of total compress time but a faithful picture of where the
    // hot loops spend their cycles.
    let rows = [
            ("CDC chunking", cdc_ns, false),
            ("Dedup lookup", dedup_ns, false),
            ("Block classify", classify_ns, false),
            ("Dict codec (v4.5/v4.8)", dict_ns, false),
            ("LZ77 encode (all blocks)", lz77_ns, true),
            ("Op stream split", op_split_ns, false),
            ("Sparse rANS (total)", sparse_rans_ns, true),
            ("  └ rANS encode core", rans_encode_ns, false),
        ];
        eprintln!();
        eprintln!("phase breakdown (ns, % of measured = {:.3} ms):", measured as f64 / 1_000_000.0);
        for (name, r, mark) in rows.iter() {
            let v = *r;
            let pct = if measured == 0 { 0.0 } else { v as f64 * 100.0 / measured as f64 };
            let marker = if *mark { "*" } else { " " };
            eprintln!("  {}{:>30} : {:>12} ns  ({:>5.1} %)", marker, name, v, pct);
        }
        // Catch-all note.
        eprintln!();
        eprintln!("  (Total measured = {:.3} ms; remaining time is header I/O, dispatch,",
         measured as f64 / 1_000_000.0);
        eprintln!("   and the per-block pick-smallest-of-4-paths wrapper.)");
}

fn pct(x: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        x as f64 * 100.0 / total as f64
    }
}