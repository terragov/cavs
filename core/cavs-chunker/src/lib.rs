//! Chunking strategies for CAVS-1.
//!
//! Two modes, per the CAVS-1 design study:
//! - `Fixed`: fixed-size chunks aligned to the payload start. Used for
//!   already-packaged media segments (fMP4/CMAF), where stable, CDN-friendly
//!   boundaries matter more than shift-resistant dedup.
//! - `Cdc`: FastCDC content-defined chunking. Used for raw assets, large
//!   bundles and screen-content payloads where redundancy crosses files and
//!   versions with insertions/shifts.

use std::ops::Range;

/// FastCDC chunk-size normalization level (see the FastCDC 2020 paper).
/// Higher levels concentrate chunk sizes around `avg` — level 3 measured
/// ~20% smaller update payloads on real builds for the small profiles —
/// but different levels produce different boundaries, so the level is part
/// of a profile's identity and must never change for published content.
/// `NORM_DEFAULT` (level 1) is what every profile before the 16k/32k ones
/// used and matches `fastcdc::v2020::FastCDC::new`.
pub const NORM_DEFAULT: u8 = 1;
/// Tight normalization (level 3), used by the `fastcdc-16k`/`fastcdc-32k`
/// profiles introduced in 1.3.0.
pub const NORM_TIGHT: u8 = 3;

/// Chunking strategy. Sizes are in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkMode {
    Fixed {
        size: usize,
    },
    /// FastCDC content-defined chunking. `norm` is the normalization level
    /// (0–3, see [`NORM_DEFAULT`]); it changes boundary placement, so it is
    /// as much a part of the profile as the sizes are.
    Cdc {
        min: usize,
        avg: usize,
        max: usize,
        norm: u8,
    },
}

impl ChunkMode {
    /// Default for packaged media segments: 256 KiB fixed.
    pub fn media_default() -> Self {
        ChunkMode::Fixed { size: 256 * 1024 }
    }

    /// Default for generic binary assets: FastCDC 16 KiB / 64 KiB / 256 KiB.
    /// The 64 KiB average won the real-corpus benchmark: 3× smaller update
    /// payloads than 256 KiB for ~+1.3% storage (see
    /// test/real-games/RESULTADOS_COMPARATIVAS.md).
    pub fn asset_default() -> Self {
        ChunkMode::Cdc {
            min: 16 * 1024,
            avg: 64 * 1024,
            max: 256 * 1024,
            norm: NORM_DEFAULT,
        }
    }

    /// Aggressive CDC for screen content / highly repetitive material:
    /// 16 KiB / 64 KiB / 256 KiB.
    pub fn screen_default() -> Self {
        ChunkMode::Cdc {
            min: 16 * 1024,
            avg: 64 * 1024,
            max: 256 * 1024,
            norm: NORM_DEFAULT,
        }
    }
}

/// Split `input` into chunk byte-ranges according to `mode`.
///
/// Ranges are contiguous, non-overlapping and cover the whole input.
/// An empty input yields no chunks.
pub fn split(input: &[u8], mode: ChunkMode) -> Vec<Range<usize>> {
    if input.is_empty() {
        return Vec::new();
    }
    match mode {
        ChunkMode::Fixed { size } => {
            let size = size.max(1);
            let mut out = Vec::with_capacity(input.len().div_ceil(size));
            let mut off = 0;
            while off < input.len() {
                let end = (off + size).min(input.len());
                out.push(off..end);
                off = end;
            }
            out
        }
        ChunkMode::Cdc {
            min,
            avg,
            max,
            norm,
        } => {
            let level = match norm {
                0 => fastcdc::v2020::Normalization::Level0,
                1 => fastcdc::v2020::Normalization::Level1,
                2 => fastcdc::v2020::Normalization::Level2,
                _ => fastcdc::v2020::Normalization::Level3,
            };
            let chunker = fastcdc::v2020::FastCDC::with_level(
                input, min as u32, avg as u32, max as u32, level,
            );
            chunker.map(|c| c.offset..c.offset + c.length).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_covers(input_len: usize, ranges: &[Range<usize>]) {
        let mut expected = 0;
        for r in ranges {
            assert_eq!(r.start, expected, "ranges must be contiguous");
            assert!(r.end > r.start, "ranges must be non-empty");
            expected = r.end;
        }
        assert_eq!(expected, input_len, "ranges must cover the whole input");
    }

    #[test]
    fn fixed_covers_input() {
        let data = vec![7u8; 1000];
        let ranges = split(&data, ChunkMode::Fixed { size: 256 });
        assert_eq!(ranges.len(), 4);
        assert_covers(data.len(), &ranges);
        assert_eq!(ranges[3], 768..1000);
    }

    #[test]
    fn fixed_exact_multiple() {
        let data = vec![1u8; 512];
        let ranges = split(&data, ChunkMode::Fixed { size: 256 });
        assert_eq!(ranges.len(), 2);
        assert_covers(data.len(), &ranges);
    }

    #[test]
    fn empty_input_no_chunks() {
        assert!(split(&[], ChunkMode::media_default()).is_empty());
        assert!(split(&[], ChunkMode::asset_default()).is_empty());
    }

    #[test]
    fn cdc_covers_input() {
        // Pseudo-random data so CDC finds boundaries.
        let mut data = vec![0u8; 3 * 1024 * 1024];
        let mut state = 0x12345678u32;
        for b in data.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        let ranges = split(&data, ChunkMode::asset_default());
        assert!(ranges.len() > 1);
        assert_covers(data.len(), &ranges);
    }

    #[test]
    fn cdc_dedups_shifted_content() {
        // Insert bytes at the front; most chunk boundaries should re-align,
        // producing many identical chunks — the core CDC property.
        let mut base = vec![0u8; 2 * 1024 * 1024];
        let mut state = 0xdeadbeefu32;
        for b in base.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        let mut shifted = vec![0xAB; 1337];
        shifted.extend_from_slice(&base);

        let mode = ChunkMode::screen_default();
        let h = |r: &Range<usize>, d: &[u8]| cavs_hash_like(&d[r.clone()]);
        let set_a: std::collections::HashSet<u64> =
            split(&base, mode).iter().map(|r| h(r, &base)).collect();
        let hits = split(&shifted, mode)
            .iter()
            .filter(|r| set_a.contains(&h(r, &shifted)))
            .count();
        assert!(hits > 0, "CDC should recover shared chunks after a shift");
    }

    /// Compatibility pin: `norm: NORM_DEFAULT` must keep producing the exact
    /// boundaries `FastCDC::new` produced before the field existed — every
    /// published version stream depends on them.
    #[test]
    fn norm_default_matches_pre_field_boundaries() {
        let mut data = vec![0u8; 4 * 1024 * 1024];
        let mut state = 0xfeedfaceu32;
        for b in data.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        let via_mode = split(&data, ChunkMode::asset_default());
        let direct: Vec<Range<usize>> =
            fastcdc::v2020::FastCDC::new(&data, 16 * 1024, 64 * 1024, 256 * 1024)
                .map(|c| c.offset..c.offset + c.length)
                .collect();
        assert_eq!(via_mode, direct);
    }

    #[test]
    fn norm_tight_covers_input_and_narrows_sizes() {
        let mut data = vec![0u8; 8 * 1024 * 1024];
        let mut state = 0xabad1deau32;
        for b in data.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        let loose = ChunkMode::Cdc {
            min: 4096,
            avg: 16384,
            max: 65536,
            norm: NORM_DEFAULT,
        };
        let tight = ChunkMode::Cdc {
            min: 4096,
            avg: 16384,
            max: 65536,
            norm: NORM_TIGHT,
        };
        let a = split(&data, loose);
        let b = split(&data, tight);
        assert_covers(data.len(), &a);
        assert_covers(data.len(), &b);
        // Tighter normalization concentrates sizes around avg: the spread
        // between the 10th and 90th percentile must shrink.
        let spread = |ranges: &[Range<usize>]| {
            let mut sizes: Vec<usize> = ranges.iter().map(|r| r.len()).collect();
            sizes.sort_unstable();
            sizes[sizes.len() * 9 / 10] - sizes[sizes.len() / 10]
        };
        assert!(
            spread(&b) < spread(&a),
            "tight spread {} !< loose spread {}",
            spread(&b),
            spread(&a)
        );
    }

    // Cheap stand-in hash for tests (avoids a dev-dependency cycle).
    fn cavs_hash_like(data: &[u8]) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
}
