#![feature(portable_simd)]

// Build with:  RUSTFLAGS="-C target-cpu=native" cargo build --release
// (x86_64 only: uses _mm_prefetch.)

use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::simd::{cmp::SimdPartialEq, u8x32};
use std::sync::atomic::{AtomicUsize, Ordering};

const TABLE_BITS: u32 = 15; // 32768 slots per thread (2 MB); try 14 and 16
const TABLE_SIZE: usize = 1 << TABLE_BITS;
const SAFE: usize = 128; // bytes that must remain for the unchecked fast path
const STREAMS: usize = 3; // interleaved streams per thread; try 2, 3, 4
const CHUNK: usize = 4 << 20; // work-stealing chunk size in bytes; try 1..16 MB

type Long = HashMap<Vec<u8>, Stats>;

#[derive(Clone, Copy)]
#[repr(C)]
struct Stats {
    sum: i64,
    count: u32,
    min: i16,
    max: i16,
}

impl Stats {
    #[inline(always)]
    fn new() -> Self {
        Stats { sum: 0, count: 0, min: i16::MAX, max: i16::MIN }
    }
    #[inline(always)]
    fn add(&mut self, t: i16) {
        self.min = self.min.min(t);
        self.max = self.max.max(t);
        self.sum += t as i64;
        self.count += 1;
    }
    fn merge(&mut self, o: &Stats) {
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.sum += o.sum;
        self.count += o.count;
    }
}

/// An all-zero Slot is a valid empty slot (len == 0); Stats are initialised
/// on first insert, so the table can come straight from anonymous mmap.
#[derive(Clone, Copy)]
#[repr(C, align(64))] // one slot == one cache line
struct Slot {
    key: u8x32, // zero-padded name
    len: u32,   // 0 == empty
    st: Stats,
}

const _: () = assert!(std::mem::size_of::<Slot>() == 64);

/// Per-thread hash table living in a 2 MB-aligned anonymous mapping with
/// MADV_HUGEPAGE, so the whole table sits behind a single TLB entry.
struct Table {
    base: *mut libc::c_void,
    map_len: usize,
    slots: *mut Slot,
}

unsafe impl Send for Table {}

impl Table {
    fn new() -> Self {
        const HUGE: usize = 2 << 20;
        let bytes = TABLE_SIZE * std::mem::size_of::<Slot>();
        let map_len = bytes + HUGE;
        unsafe {
            let base = libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if base == libc::MAP_FAILED {
                panic!("{:?}", std::io::Error::last_os_error());
            }
            let aligned = (base as usize + HUGE - 1) & !(HUGE - 1);
            // Best effort: ignore failure (THP may be disabled).
            libc::madvise(aligned as *mut _, bytes, libc::MADV_HUGEPAGE);
            Table { base, map_len, slots: aligned as *mut Slot }
        }
    }
    #[inline(always)]
    fn as_mut(&mut self) -> &mut [Slot] {
        unsafe { std::slice::from_raw_parts_mut(self.slots, TABLE_SIZE) }
    }
    fn as_slice(&self) -> &[Slot] {
        unsafe { std::slice::from_raw_parts(self.slots, TABLE_SIZE) }
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, self.map_len);
        }
    }
}

fn map_file(f: &File) -> &'static [u8] {
    let len = f.metadata().unwrap().len() as usize;
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE, // try: libc::MAP_PRIVATE | libc::MAP_POPULATE
            f.as_raw_fd(),
            0,
        );
        if ptr == libc::MAP_FAILED {
            panic!("{:?}", std::io::Error::last_os_error());
        }
        // No MADV_SEQUENTIAL: many threads read different regions at once.
        // Experiment with libc::MADV_HUGEPAGE / MADV_WILLNEED here.
        std::slice::from_raw_parts(ptr as *const u8, len)
    }
}

/// One multiply over the first 16 bytes. Collisions only cost an extra probe,
/// since the full 32-byte key is still compared.
#[inline(always)]
fn hash(name: u8x32) -> u64 {
    let w: [u64; 4] = unsafe { std::mem::transmute(name) };
    (w[0] ^ w[1].rotate_left(32)).wrapping_mul(0x9e3779b97f4a7c15)
}

#[inline(always)]
fn slot_index(name: u8x32) -> usize {
    (hash(name) >> (64 - TABLE_BITS)) as usize // top bits mix best
}

/// Probe from `idx` (already hashed) and update / insert.
#[inline(always)]
fn update_at(table: &mut [Slot], mut idx: usize, name: u8x32, len: usize, temp: i16) {
    loop {
        let s = unsafe { table.get_unchecked_mut(idx) };
        if s.key == name {
            s.st.add(temp);
            return;
        }
        if s.len == 0 {
            s.key = name;
            s.len = len as u32;
            s.st = Stats::new();
            s.st.add(temp);
            return;
        }
        idx = (idx + 1) & (TABLE_SIZE - 1);
    }
}

#[inline(always)]
fn update(table: &mut [Slot], name: u8x32, len: usize, temp: i16) {
    update_at(table, slot_index(name), name, len, temp);
}

/// 32 bytes of 0xFF followed by 32 zero bytes. Loading 32 bytes at offset
/// (32 - len) yields exactly `len` 0xFF bytes followed by zeros.
static MASKS: [u8; 64] = {
    let mut m = [0u8; 64];
    let mut i = 0;
    while i < 32 {
        m[i] = 0xFF;
        i += 1;
    }
    m
};

/// Zero everything past `len` so equal names give equal vectors. 1 <= len <= 31.
#[inline(always)]
unsafe fn mask_name(v: u8x32, len: usize) -> u8x32 {
    let m = std::ptr::read_unaligned(MASKS.as_ptr().add(32 - len) as *const u8x32);
    v & m
}

/// Branchless parse of "-?d?d.d\n" from 8 little-endian bytes.
/// Returns (temp * 10, bytes consumed including the '\n').
#[inline(always)]
fn parse_temp(word: u64) -> (i16, usize) {
    const DOT_BITS: u64 = 0x10101000;
    let inv = !word;
    let dot_pos = (inv & DOT_BITS).trailing_zeros(); // 12, 20 or 28
    let signed = (((inv << 59) as i64) >> 63) as u64; // all ones if first byte is '-'
    let design_mask = !(signed & 0xFF);
    let digits = ((word & design_mask) << (28 - dot_pos)) & 0x0F000F0F00;
    let abs = (digits.wrapping_mul(0x640a0001) >> 32) & 0x3FF;
    let value = (abs ^ signed).wrapping_sub(signed) as i64;
    (value as i16, (dot_pos >> 3) as usize + 3)
}

/// Bounds-checked parser for the last few lines of the file.
#[inline(always)]
fn parse_temp_and_semi(line: &[u8]) -> (i16, usize) {
    let n = line.len();
    let mut temp = (line[n - 1] - b'0') as i16 + (line[n - 3] - b'0') as i16 * 10;
    let b2 = line[n - 4];
    let semi_pos = if b2 == b';' {
        n - 4
    } else if b2 == b'-' {
        temp = -temp;
        n - 5
    } else {
        temp += (b2 - b'0') as i16 * 100;
        if line[n - 5] == b';' {
            n - 5
        } else {
            temp = -temp;
            n - 6
        }
    };
    (temp, semi_pos)
}

/// Slow, general path (file tail and names >= 32 bytes).
fn record(table: &mut [Slot], long: &mut Long, name: &[u8], temp: i16) {
    if name.len() <= 32 {
        let mut buf = [0u8; 32];
        buf[..name.len()].copy_from_slice(name);
        update(table, u8x32::from_array(buf), name.len(), temp);
    } else {
        long.entry(name.to_vec()).or_insert_with(Stats::new).add(temp);
    }
}

/// A parsed line whose slot has been prefetched but not yet updated.
#[derive(Clone, Copy)]
struct Pending {
    name: u8x32,
    len: u32, // 0 == nothing pending
    temp: i16,
    idx: u32,
}

impl Pending {
    #[inline(always)]
    fn none() -> Self {
        Pending { name: u8x32::splat(0), len: 0, temp: 0, idx: 0 }
    }
}

/// Parse one line, prefetch its slot, return it for a later `update_at`.
/// Names >= 32 bytes are recorded immediately and return `none()`.
/// Caller guarantees *at + SAFE <= len.
#[inline(always)]
unsafe fn parse(
    base: *const u8,
    len: usize,
    at: &mut usize,
    table: *const Slot,
    tbl: &mut [Slot],
    long: &mut Long,
) -> Pending {
    let p = base.add(*at);
    let v: u8x32 = std::ptr::read_unaligned(p as *const u8x32);
    let semi_bits = v.simd_eq(u8x32::splat(b';')).to_bitmask();

    if semi_bits != 0 {
        let nlen = semi_bits.trailing_zeros() as usize;
        let word = std::ptr::read_unaligned(p.add(nlen + 1) as *const u64);
        let (temp, used) = parse_temp(word);
        let name = mask_name(v, nlen);
        let idx = slot_index(name);
        _mm_prefetch::<_MM_HINT_T0>(table.add(idx) as *const i8);
        *at += nlen + 1 + used;
        Pending { name, len: nlen as u32, temp, idx: idx as u32 }
    } else {
        // name of 32+ bytes (rare)
        let semi = libc::memchr(p.add(32) as *const _, b';' as i32, len - *at - 32) as *const u8;
        let nlen = semi as usize - p as usize;
        let word = std::ptr::read_unaligned(semi.add(1) as *const u64);
        let (temp, used) = parse_temp(word);
        record(tbl, long, std::slice::from_raw_parts(p, nlen), temp);
        *at += nlen + 1 + used;
        Pending::none()
    }
}

/// One line, unpipelined (used to drain streams). Caller guarantees *at + SAFE <= len.
#[inline(always)]
unsafe fn step(base: *const u8, len: usize, at: &mut usize, table: &mut [Slot], long: &mut Long) {
    let ptr = table.as_ptr();
    let n = parse(base, len, at, ptr, table, long);
    if n.len != 0 {
        update_at(table, n.idx as usize, n.name, n.len as usize, n.temp);
    }
}

/// Position of the first line start at or after `nominal`
/// (i.e. just after the first '\n' at index >= nominal - 1).
fn line_start(map: &[u8], nominal: usize) -> usize {
    let len = map.len();
    if nominal == 0 {
        return 0;
    }
    if nominal >= len {
        return len;
    }
    let mut p = nominal - 1;
    while p < len && map[p] != b'\n' {
        p += 1;
    }
    (p + 1).min(len)
}

/// Process whole lines in map[start..end]. `start` must be a line start and
/// `end` must be just after a '\n' (or the end of the file).
fn process_chunk(map: &'static [u8], start: usize, end: usize, table: &mut [Slot], long: &mut Long) {
    let len = map.len();
    let base = map.as_ptr();

    // Move a position forward to just after the next '\n', never past `end`.
    let align = |pos: usize| -> usize {
        let mut p = pos;
        while p < end && map[p] != b'\n' {
            p += 1;
        }
        (p + 1).min(end)
    };

    // Split the range into STREAMS line-aligned sub-ranges.
    let mut at = [start; STREAMS];
    let mut stop = [end; STREAMS];
    for i in 1..STREAMS {
        let guess = start + (end - start) / STREAMS * i;
        at[i] = align(guess).max(at[i - 1]);
    }
    for i in 0..STREAMS - 1 {
        stop[i] = at[i + 1];
    }

    // Fast path may only run while SAFE bytes remain in the whole file.
    let limit = len.saturating_sub(SAFE);
    let lim = stop.map(|e| e.min(limit));

    let mut pend = [Pending::none(); STREAMS];
    let tptr = table.as_ptr();

    unsafe {
        // Streams advance together; each stream parses+prefetches line N
        // while updating line N-1, so table misses overlap with useful work.
        'lockstep: loop {
            for i in 0..STREAMS {
                if at[i] >= lim[i] {
                    break 'lockstep;
                }
            }
            for i in 0..STREAMS {
                let n = parse(base, len, &mut at[i], tptr, table, long);
                let p = pend[i];
                if p.len != 0 {
                    update_at(table, p.idx as usize, p.name, p.len as usize, p.temp);
                }
                pend[i] = n;
            }
        }
        // Flush the pipeline.
        for i in 0..STREAMS {
            let p = pend[i];
            if p.len != 0 {
                update_at(table, p.idx as usize, p.name, p.len as usize, p.temp);
            }
        }
        // Finish whichever streams still have fast-path lines left.
        for i in 0..STREAMS {
            while at[i] < lim[i] {
                step(base, len, &mut at[i], table, long);
            }
        }
    }

    // Bounds-checked tail (only matters for the last chunk of the file).
    for i in 0..STREAMS {
        while at[i] < stop[i] {
            let rest = &map[at[i]..];
            let nl = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
            let line = &rest[..nl];
            at[i] += nl + 1;
            if line.is_empty() {
                continue;
            }
            let (temp, semi) = parse_temp_and_semi(line);
            record(table, long, &line[..semi], temp);
        }
    }
}

/// Each worker owns one table for its whole life and pulls chunks from a
/// shared counter, so a slow thread never holds up the rest.
fn worker(map: &'static [u8], next: &AtomicUsize) -> (Table, Long) {
    let len = map.len();
    let mut table = Table::new();
    let mut long: Long = HashMap::new();
    loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        let nominal = i.saturating_mul(CHUNK);
        if nominal >= len {
            break;
        }
        let start = line_start(map, nominal);
        let end = line_start(map, nominal.saturating_add(CHUNK));
        if start < end {
            process_chunk(map, start, end, table.as_mut(), &mut long);
        }
    }
    (table, long)
}

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let map = map_file(&f);

    // Try physical-core count vs logical-core count.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let next = AtomicUsize::new(0);

    let results: Vec<(Table, Long)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| worker(map, &next))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge per-thread tables (~10k entries each, negligible next to the scan).
    let mut merged: Long = HashMap::new();
    for (table, long) in &results {
        for s in table.as_slice().iter().filter(|s| s.len != 0) {
            let name = s.key.to_array()[..s.len as usize].to_vec();
            merged.entry(name).or_insert_with(Stats::new).merge(&s.st);
        }
        for (name, st) in long {
            merged.entry(name.clone()).or_insert_with(Stats::new).merge(st);
        }
    }

    let mut out: Vec<(Vec<u8>, Stats)> = merged.into_iter().collect();
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    print!("{{");
    for (i, (name, st)) in out.iter().enumerate() {
        if i > 0 {
            print!(", ");
        }
        print!(
            "{}={:.1}/{:.1}/{:.1}",
            String::from_utf8_lossy(name),
            st.min as f64 / 10.0,
            (st.sum as f64 / st.count as f64) / 10.0,
            st.max as f64 / 10.0,
        );
    }
    println!("}}");
}