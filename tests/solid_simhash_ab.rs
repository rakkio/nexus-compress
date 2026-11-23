//! Sprint 5.7.x — A/B comparison of NXS6 solid compression with vs
//! without the new SimHash + content-aware pipeline.
//!
//! We can't really disable the new pipeline at runtime without
//! feature flags, so this test instead exercises both code paths:
//! - **Old behavior**: simulated via direct XzEncoder over a flat
//!   concatenation of bytes (no minify, no SimHash sort, no
//!   content-aware routing).
//! - **New behavior**: `compress_with_progress` (current production
//!   code path with all the new heuristics).
//!
//! Both run on the same corpus. We compare archive size + time and
//! print the delta. The numbers should consistently favor the new
//! pipeline, but the magnitude depends on the corpus shape.
//!
//! Note: this isn't a strict "apples-to-apples" because the new
//! pipeline also runs minify() per file. To make the comparison
//! fairer, we feed the OLD path the already-minified bytes — so
//! the only difference is the sort order and the random-byte
//! routing.

use std::time::Instant;
use std::io::Write;
use xz2::write::XzEncoder;

/// Tiny lzma stream factory (mirrors `crate::ram::lzma_stream_for_requested_preset`
/// but exposed here so the test doesn't depend on internal API surface).
fn lzma_preset(level: u32) -> u32 {
    level.min(9)
}

fn run_old_pipeline(files: &[(String, Vec<u8>)], lzma_level: u32) -> (Vec<u8>, std::time::Duration) {
    // Simulates the **production OLD** behavior (before Sprint
    // 5.7.x):
    //   - .ts/.tsx → SwcAst (lossy AST minify)
    //   - everything else → Conservative minify
    //   - Files concatenated in INPUT order (no SimHash sort)
    //   - Single LZMA stream over the concatenated bytes
    //   - No content-aware routing (random .png files still go
    //     through Conservative, which is a no-op for binary).
    //
    // This matches the pre-Sprint-5.7.x production code path so
    // the comparison is apples-to-apples.
    let start = Instant::now();
    let mut solid: Vec<u8> = Vec::new();
    for (name, bytes) in files {
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let pre = match ext.as_str() {
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => {
                nexus_compress::ast_minify::minify(bytes).bytes
            }
            _ => nexus_compress::minify::minify(bytes),
        };
        solid.extend_from_slice(&pre);
    }
    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = XzEncoder::new(&mut out, lzma_preset(lzma_level));
        enc.write_all(&solid).unwrap();
        enc.finish().unwrap();
    }
    (out, start.elapsed())
}

#[test]
fn ab_compare_simhash_impact() {
    // Build a larger, more realistic corpus so the SimHash sort
    // has room to actually improve LZMA match rate.
    let mut corpus: Vec<(String, Vec<u8>)> = Vec::new();

    // 200 .tsx files, each ~10-12 KB, sharing a heavy boilerplate
    // preamble so SimHash clusters them.
    let shared = "import { useState, useMemo, useCallback } from 'react';\n\
                  import { fetchUser, ApiError, User } from './api';\n\
                  import { logger } from './logger';\n\
                  import { Button, Card, Modal } from './ui';\n\n\
                  interface Props {\n  userId: string;\n  fallback: React.ReactNode;\n}\n\n";
    for i in 0..200 {
        let body = format!(
            "export function Component{i}() {{\n\
             const [data, setData] = useState<User | null>(null);\n\
             const memoized = useMemo(() => fetchUser({i}), [{i}]);\n\
             const onClick = useCallback(() => logger.info('click', {i}), []);\n\
             return <Card title=\"Component {i}\">\n  <Button onClick={{onClick}}>{{memoized?.name}}</Button>\n  </Card>;\n\
             }}\n",
            i = i
        );
        corpus.push((
            format!("src/components/Component{}.tsx", i),
            (shared.repeat(4) + body.as_str()).into_bytes(),
        ));
    }

    // 10 .json config files.
    for i in 0..10 {
        let s = format!(
            r#"{{
  "name": "package-{}",
  "version": "1.0.{}",
  "dependencies": {{
    "react": "^18.0.0",
    "next": "^13.0.0",
    "swc": "^1.0.0",
    "typescript": "^5.0.0"
  }},
  "scripts": {{
    "dev": "next dev",
    "build": "next build",
    "start": "next start"
  }},
  "config": {{
    "compilerOptions": {{
      "target": "es2020",
      "strict": true,
      "module": "esnext"
    }}
  }}
}}"#,
            i, i
        );
        corpus.push((format!("packages/pkg{}/package.json", i), s.into_bytes()));
    }

    // 20 random PNGs (high entropy).
    for i in 0..20 {
        let mut data = vec![0u8; 16 * 1024];
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
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&data);
        corpus.push((format!("public/assets/sprite{}.png", i), png));
    }

    // 5 README files.
    for i in 0..5 {
        let md = format!(
            "# Project {}\n\nThis is a sample README.\n\n## Installation\n\
             ```bash\nnpm install\n```\n\n## Usage\n\
             ```tsx\nimport {{ Component }} from './Component';\n```\n\n{}\n",
            i,
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(40)
        );
        corpus.push((format!("docs/README{}.md", i), md.into_bytes()));
    }

    let total: u64 = corpus.iter().map(|(_, b)| b.len() as u64).sum();

    // Old path.
    let (old_archive, old_time) = run_old_pipeline(&corpus, 6);
    let old_ratio = total as f64 / old_archive.len() as f64;

    // New path.
    let start = Instant::now();
    let new_archive =
        nexus_compress::solid_archive::compress_with_progress(&corpus, 6, |_, _, _| {})
            .expect("compress_with_progress");
    let new_time = start.elapsed();
    let new_ratio = total as f64 / new_archive.len() as f64;

    let size_delta_pct = (new_archive.len() as f64 / old_archive.len() as f64 - 1.0) * 100.0;
    let ratio_delta_pct = (new_ratio - old_ratio) / old_ratio * 100.0;

    eprintln!(
        "A/B compare (corpus {} files, {:.2} MB):\n\
         - Old pipeline (concat in input order, no content routing):\n\
         \x20   archive={:.2} MB, ratio={:.2}x, saved={:.1}%, time={:.2} s\n\
         - New pipeline (SimHash sort + content-aware):\n\
         \x20   archive={:.2} MB, ratio={:.2}x, saved={:.1}%, time={:.2} s\n\
         Delta: archive {:+.1}%, ratio {:+.1}%",
        corpus.len(),
        total as f64 / 1024.0 / 1024.0,
        old_archive.len() as f64 / 1024.0 / 1024.0,
        old_ratio,
        (1.0 - old_archive.len() as f64 / total as f64) * 100.0,
        old_time.as_secs_f64(),
        new_archive.len() as f64 / 1024.0 / 1024.0,
        new_ratio,
        (1.0 - new_archive.len() as f64 / total as f64) * 100.0,
        new_time.as_secs_f64(),
        size_delta_pct,
        ratio_delta_pct,
    );

    let (entries, _solid) =
        nexus_compress::solid_archive::decompress(&new_archive).expect("decompress");
    assert_eq!(entries.len(), corpus.len());

    // We expect the new pipeline to be at least competitive
    // (within ±10% of the old). Real win on big corpuses with
    // shared structure; marginal on small / random-heavy ones.
    assert!(
        size_delta_pct < 10.0,
        "new pipeline regressed too much ({:+.1}%); investigate",
        size_delta_pct
    );
}