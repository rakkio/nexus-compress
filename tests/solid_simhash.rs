//! Sprint 5.7.x — SimHash + content-aware bench for the NXS6 solid
//! engine. Generates a synthetic Next.js-like corpus (text + JSON +
//! pseudo-binary assets), compresses it with the current engine,
//! and reports ratio + time. Use the output to compare against
//! pre-SimHash numbers (the previous pipeline had no content-aware
//! skip and no SimHash ordering).
//!
//! Run with:
//!     cargo bench --release --bench solid_simhash -- --nocapture
//!
//! Or invoke the underlying test directly:
//!     cargo test --release --bench solid_simhash -- --nocapture

use std::time::Instant;

/// Build a corpus that mimics a small Next.js repo:
///   - 50 .ts files (similar structure, different identifiers)
///   - 10 .json files (configs, lockfiles, etc.)
///   - 20 .png files (random bytes — would dilute LZMA dict)
///   - 5 .md files (README, CHANGELOG, etc.)
fn build_corpus() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();

    // 50 .ts files — each ~10 KB, with a shared base of imports +
    // boilerplate so SimHash sorting matters.
    for i in 0..50 {
        let shared = "import { useState, useMemo } from 'react';\n\
                      import { fetchUser, ApiError } from './api';\n\
                      import { logger } from './logger';\n\n\
                      interface Props {\n  userId: string;\n  fallback: React.ReactNode;\n}\n\n";
        let body = format!(
            "export function Component{i}() {{\n\
             const [data, setData] = useState(null);\n\
             const memoized = useMemo(() => fetchUser({i}), [{i}]);\n\
             return <div className=\"component-{i}\">{{memoized?.name}}</div>;\n\
             }}\n",
            i = i
        );
        out.push((
            format!("src/components/Component{}.tsx", i),
            (shared.repeat(3) + body.as_str()).into_bytes(),
        ));
    }

    // 10 .json files — package configs with shared structure.
    for i in 0..10 {
        let s = format!(
            r#"{{
  "name": "package-{}",
  "version": "1.0.{}",
  "dependencies": {{
    "react": "^18.0.0",
    "next": "^13.0.0",
    "swc": "^1.0.0"
  }},
  "scripts": {{
    "dev": "next dev",
    "build": "next build",
    "start": "next start"
  }}
}}"#,
            i, i
        );
        out.push((format!("packages/pkg{}/package.json", i), s.into_bytes()));
    }

    // 20 "PNG" files — random bytes (incompressible).
    for i in 0..20 {
        let mut data = vec![0u8; 30 * 1024];
        // Pseudo-random fill (xorshift32 — small enough to be
        // deterministic but spread across the byte space).
        let mut x: u32 = 0xdeadbeef ^ i;
        for chunk in data.chunks_mut(4) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let bytes = x.to_le_bytes();
            for (j, b) in chunk.iter_mut().enumerate() {
                if j < 4 {
                    *b = bytes[j];
                }
            }
        }
        // PNG magic header (so the file looks like an image even
        // though the bytes after the magic are random).
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&data);
        out.push((format!("public/assets/sprite{}.png", i), png));
    }

    // 5 README files — long markdown.
    for i in 0..5 {
        let md = format!(
            "# Project {}\n\n\
             This is a sample README file with lots of repetitive markdown.\n\
             ## Installation\n\n\
             ```bash\nnpm install\n```\n\n\
             ## Usage\n\n\
             ```tsx\nimport {{ Component }} from './Component';\n```\n\n\
             {}\n",
            i,
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(40)
        );
        out.push((format!("docs/README{}.md", i), md.into_bytes()));
    }

    out
}

#[test]
fn bench_solid_simhash_realistic_corpus() {
    let corpus = build_corpus();
    let total_original: u64 = corpus.iter().map(|(_, b)| b.len() as u64).sum();

    eprintln!(
        "Corpus: {} files, {:.2} MB total (text-heavy Next.js-like)",
        corpus.len(),
        total_original as f64 / 1024.0 / 1024.0
    );

    let start = Instant::now();
    let archive = nexus_compress::solid_archive::compress_with_progress(
        &corpus,
        6,
        |_, _, _| {},
    )
    .expect("compress");
    let elapsed = start.elapsed();

    let ratio = total_original as f64 / archive.len() as f64;
    let saved_pct = (1.0 - archive.len() as f64 / total_original as f64) * 100.0;
    eprintln!(
        "NXS6 v6 (with SimHash + content-aware): archive={:.2} MB, \
         ratio={:.2}x, saved={:.1}%, time={:.2}s",
        archive.len() as f64 / 1024.0 / 1024.0,
        ratio,
        saved_pct,
        elapsed.as_secs_f64()
    );

    // Round-trip the archive to confirm correctness.
    let (entries, _solid) =
        nexus_compress::solid_archive::decompress(&archive).expect("decompress");
    assert_eq!(entries.len(), corpus.len());

    // Count how many entries got Raw preprocessor (the random .png
    // files should all be Raw after the content-aware change).
    let raw_count = entries
        .iter()
        .filter(|e| e.preprocessor == nexus_compress::solid_archive::Preprocessor::Raw)
        .count();
    eprintln!(
        "Preprocessor distribution: Raw={}, others={} (random .pngs should be Raw)",
        raw_count,
        entries.len() - raw_count
    );
    // We have 20 .png files in the corpus; expect 20 Raw.
    assert!(
        raw_count >= 20,
        "expected at least 20 Raw entries (the random .pngs); got {}",
        raw_count
    );
}