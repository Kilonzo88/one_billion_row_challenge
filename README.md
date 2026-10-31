# 🔥 One Billion Row Challenge — Rust Edition

Processing **1,000,000,000** temperature measurements as fast as possible, in Rust.

> Inspired by Gunnar Morling's [One Billion Row Challenge](https://github.com/gunnarmorling/1brc), originally designed for Java.

## The Result

| # | Branch | Technique | Mean Time | Speedup vs Previous | Total Speedup |
|---|--------|-----------|----------:|--------------------:|--------------:|
| 01 | [`01-naive-baseline`](../../tree/01-naive-baseline) | Naïve `BufRead` + `HashMap<String, _>` | **6.2 min** | — | **1.0×** |
| 02 | [`02-compiler-optimizations`](../../tree/02-compiler-optimizations) | `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` | **3.5 min** | 1.8× | **1.8×** |
| 03 | [`03-reduce-allocations`](../../tree/03-reduce-allocations) | Eliminate per-line `String` allocations | **1.1 min** | 3.2× | **5.6×** |
| 04 | [`04-unchecked-utf8`](../../tree/04-unchecked-utf8) | Skip UTF-8 validation (`from_utf8_unchecked`) | **1.1 min** | ~1.0× | **5.6×** |
| 05 | [`05-mmap-io`](../../tree/05-mmap-io) | Memory-mapped I/O (`mmap`) instead of buffered reads | **42.9 s** | 1.5× | **8.7×** |
| 06 | [`06-manual-int-parsing`](../../tree/06-manual-int-parsing) | Hand-rolled integer parsing (skip `str::parse`) | **38.7 s** | 1.1× | **9.6×** |
| 07 | [`07-memchr-delimiters`](../../tree/07-memchr-delimiters) | `memchr` for delimiter scanning | **35.4 s** | 1.1× | **10.5×** |
| 08 | [`08-optimizing-hashmap`](../../tree/08-optimizing-hashmap) | Custom hash map with inlined keys | **29.1 s** | 1.2× | **12.8×** |
| 09 | [`09-optimized-next<u8>`](../../tree/09-optimized-next%3Cu8%3E) | Optimized byte iterator (`next()` inlining) | **28.2 s** | 1.0× | **13.2×** |
| 10 | [`10-removed-pointer-dereferencing-overhead`](../../tree/10-removed-pointer-dereferencing-overhead) | Fixed-array station keys (no heap pointers) | **28.4 s** | ~1.0× | **13.1×** |
| 11 | [`11-branchless-int-parsing`](../../tree/11-branchless-int-parsing) | Branchless temperature parsing | **27.8 s** | 1.0× | **13.4×** |
| 12 | [`12-conc_temp_semi-colon`](../../tree/12-conc_temp_semi-colon) | Concurrent semicolon + temperature parsing | **24.9 s** | 1.1× | **14.9×** |
| 13 | [`13-flat-table`](../../tree/13-flat-table) | Flat 64-byte cache-line-aligned hash table slots | **12.1 s** | 2.1× | **30.7×** |
| 14 | [`14-interleaved-multi-stream-execution`](../../tree/14-interleaved-multi-stream-execution) | SIMD semicolon scan, SWAR temp parse, 3-stream ILP with prefetch | **7.9 s** | 1.5× | **47.1×** |
| 15 | [`15-multi-threading`](../../tree/15-multi-threading) | Multi-threaded with atomic work-stealing across all CPU cores | **2.2 s** | 3.6× | **169.1×** |

> **169× faster** — from 6.2 minutes down to 2.2 seconds.

All benchmarks were run on the same machine using [`hyperfine`](https://github.com/sharkdp/hyperfine) with 3 runs and 1 warmup.

## Optimization Journey

### Phase 1 — Low-Hanging Fruit (6.2 min → 42.9 s)

**Compiler flags** (`lto`, `codegen-units = 1`, `panic = "abort"`) gave an immediate 1.8× boost by enabling whole-program optimization and eliminating unwind tables.

**Reducing allocations** was the single biggest early win — the naïve baseline allocated a new `String` for every line. Reusing buffers and avoiding per-line heap allocations cut runtime by 3.2×.

**Memory-mapped I/O** replaced `BufReader` with `mmap`, eliminating syscall overhead and letting the kernel page-fault data in on demand. This also set the stage for all later optimizations by giving us a raw byte slice to work with.

### Phase 2 — Parsing & Hashing (42.9 s → 24.9 s)

**Manual integer parsing** replaced `str::parse::<f64>()` with hand-written byte-level arithmetic, since temperatures always have exactly one decimal digit.

**Custom hash map** with fixed-size inlined keys eliminated the double indirection of `HashMap<String, _>` — each bucket holds the station name directly, so a lookup touches one contiguous block of memory.

**Concurrent semicolon + temperature parsing** merged two sequential scans (find `;`, then parse the number) into a single pass over each line.

### Phase 3 — Cache & ILP (24.9 s → 7.9 s)

**Flat cache-line-aligned table** — each slot is exactly 64 bytes (`#[repr(C, align(64))]`), holding the SIMD key, length, and stats in one cache line. A lookup that hits is a single cache-line fetch.

**SIMD semicolon scanning** — a 32-byte `portable_simd` load + `simd_eq` finds the `;` delimiter in one instruction, replacing a byte-by-byte scan.

**Branchless SWAR temperature parsing** — 8 bytes are loaded as a `u64` and the sign, digits, and decimal point are extracted with bit manipulation (no branches, no loops).

**3-stream interleaved execution with prefetch** — the file is split into 3 sub-ranges processed in lockstep. Each stream issues `_mm_prefetch` for its *next* hash table slot while the *current* slot is being updated, hiding L2/L3 cache miss latency through instruction-level parallelism.

### Phase 4 — Multi-Threading (7.9 s → 2.2 s)

**Scoped worker threads** (`std::thread::scope`) are spawned to match `available_parallelism()`.

**Atomic work-stealing** — an `AtomicUsize` counter divides the file into 4 MB chunks. Each thread calls `fetch_add(1, Relaxed)` to claim the next chunk, ensuring fast cores process more chunks and no core sits idle.

**Per-thread hash tables** — each thread owns a private 2 MB huge-page-backed table (via `mmap` + `MADV_HUGEPAGE`). Zero locks, zero contention during the hot loop. Tables are merged on the main thread after all workers finish, which takes < 2 ms for ~10K unique stations.

## Architecture

```
measurements.txt (13 GB)
        │
        ▼
   ┌─── mmap (read-only, MAP_PRIVATE) ───┐
   │                                      │
   │  Shared memory-mapped byte slice     │
   │  (all threads read concurrently)     │
   └──────────────────────────────────────┘
        │
        ├── Thread 0 ──► AtomicUsize::fetch_add ──► chunk [0..4MB) ──► private Table
        ├── Thread 1 ──► AtomicUsize::fetch_add ──► chunk [4..8MB) ──► private Table
        ├── Thread 2 ──► AtomicUsize::fetch_add ──► chunk [8..12MB) ──► private Table
        ⋮
        └── Thread N ──► ...
                                   │
                                   ▼
                         ┌─── Merge Phase ───┐
                         │  ~10k stations    │
                         │  < 2 ms           │
                         └───────────────────┘
                                   │
                                   ▼
                          Sorted output to stdout
```

## Building & Running

**Prerequisites**: Rust nightly (for `#![feature(portable_simd)]`), Linux x86_64.

```bash
# Generate the 1 billion row input file
cargo run --release --bin gen

# Build with native CPU optimizations
RUSTFLAGS="-C target-cpu=native" cargo build --release

# Run
./target/release/one_billion_row_challenge

# Benchmark
hyperfine --warmup 1 --runs 3 './target/release/one_billion_row_challenge'
```

Or, if using the `.cargo/config.toml` (already configured in this repo):

```bash
cargo build --release
./target/x86_64-unknown-linux-gnu/release/one_billion_row_challenge
```

### Cargo Profile

```toml
[profile.release]
lto = "fat"
panic = "abort"
debug = true        # enables perf profiling with symbols
codegen-units = 1
```

## Branch Structure

Each branch represents one incremental optimization step. You can check out any branch to see the code at that stage and compare performance:

```bash
git checkout 01-naive-baseline     # start here
git checkout 08-optimizing-hashmap # custom hash map
git checkout 13-flat-table         # cache-line slots
git checkout 15-multi-threading    # final version
```

Every branch contains a `TIME.md` with its `hyperfine` benchmark results.

## Key Lessons

1. **Allocations dominate naïve code.** Eliminating per-line `String` allocations gave the largest single-step improvement (3.2×).
2. **Cache locality is king.** The flat 64-byte slot table (branch 13) delivered a 2.1× improvement by ensuring one hash table lookup = one cache-line fetch.
3. **ILP hides latency.** Interleaving 3 independent streams with prefetch (branch 14) cut runtime by 1.5× on a single core — the CPU's out-of-order engine overlaps cache misses with useful computation.
4. **Multi-threading scales linearly** when there's no shared mutable state. Per-thread tables with atomic chunk stealing achieved ~3.6× on 8 cores (4 physical + HT).
5. **SIMD isn't always the answer.** Vectorized semicolon scanning (32-byte `simd_eq`) helped, but attempting SIMD on short lines (<64 bytes) showed diminishing returns and added FFI overhead.
6. **Profile before optimizing.** `perf record -F 999 -g` guided every decision — without it, we would have optimized the wrong things.

## License

This project is provided as-is for educational purposes.
