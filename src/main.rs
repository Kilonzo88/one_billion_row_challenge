#![feature(portable_simd)]

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::simd::{cmp::SimdPartialEq, u8x32, Mask, Select};

const TABLE_BITS: u32 = 15; // 32768 slots; try 14 and 16 too
const TABLE_SIZE: usize = 1 << TABLE_BITS;
const SAFE: usize = 128; // bytes that must remain for the unchecked fast path
const STREAMS: usize = 3; // try 2, 3, 4

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
}

#[derive(Clone, Copy)]
#[repr(C, align(64))] // one slot == one cache line
struct Slot {
    key: u8x32, // zero-padded name
    len: u32,   // 0 == empty
    st: Stats,
}

impl Slot {
    fn empty() -> Self {
        Slot { key: u8x32::splat(0), len: 0, st: Stats::new() }
    }
}

fn mmap(f: &File) -> &'static [u8] {
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
        // Several sequential streams still read forward, so this hint is fine.
        // Try removing it or using MADV_HUGEPAGE and compare.
        libc::madvise(ptr, len, libc::MADV_SEQUENTIAL);
        std::slice::from_raw_parts(ptr as *const u8, len)
    }
}

/// One multiply over the first 16 bytes. Most names fit in 16 bytes, so this
/// keeps the dependency chain before the slot load short. Collisions on names
/// sharing a 16-byte prefix only cost an extra probe; the full 32-byte key is
/// still compared, so correctness is unaffected.
#[inline(always)]
fn hash(name: u8x32) -> u64 {
    let w: [u64; 4] = unsafe { std::mem::transmute(name) };
    (w[0] ^ w[1].rotate_left(32)).wrapping_mul(0x9e3779b97f4a7c15)
}

#[inline(always)]
fn update(table: &mut [Slot], name: u8x32, len: usize, temp: i16) {
    let mut idx = (hash(name) >> (64 - TABLE_BITS)) as usize; // top bits mix best
    loop {
        let s = unsafe { table.get_unchecked_mut(idx) };
        if s.key == name {
            s.st.add(temp);
            return;
        }
        if s.len == 0 {
            s.key = name;
            s.len = len as u32;
            s.st.add(temp);
            return;
        }
        idx = (idx + 1) & (TABLE_SIZE - 1);
    }
}

/// Zero everything past `len` so equal names give equal vectors. len <= 31 here.
#[inline(always)]
fn mask_name(v: u8x32, len: usize) -> u8x32 {
    Mask::<i8, 32>::from_bitmask((1u64 << len) - 1).select(v, u8x32::splat(0))
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

/// Slow, general path (file tail and names > 32 bytes).
fn record(table: &mut [Slot], long: &mut Long, name: &[u8], temp: i16) {
    if name.len() <= 32 {
        let mut buf = [0u8; 32];
        buf[..name.len()].copy_from_slice(name);
        update(table, u8x32::from_array(buf), name.len(), temp);
    } else {
        long.entry(name.to_vec()).or_insert_with(Stats::new).add(temp);
    }
}

/// One line on the fast path. Caller guarantees *at + SAFE <= len.
#[inline(always)]
unsafe fn step(base: *const u8, len: usize, at: &mut usize, table: &mut [Slot], long: &mut Long) {
    let p = base.add(*at);
    let v: u8x32 = std::ptr::read_unaligned(p as *const u8x32);
    let semi_bits = v.simd_eq(u8x32::splat(b';')).to_bitmask();

    if semi_bits != 0 {
        let nlen = semi_bits.trailing_zeros() as usize;
        let word = std::ptr::read_unaligned(p.add(nlen + 1) as *const u64);
        let (temp, used) = parse_temp(word);
        update(table, mask_name(v, nlen), nlen, temp);
        *at += nlen + 1 + used;
    } else {
        // name longer than 32 bytes (rare)
        let semi = libc::memchr(p.add(32) as *const _, b';' as i32, len - *at - 32) as *const u8;
        let nlen = semi as usize - p as usize;
        let word = std::ptr::read_unaligned(semi.add(1) as *const u64);
        let (temp, used) = parse_temp(word);
        record(table, long, std::slice::from_raw_parts(p, nlen), temp);
        *at += nlen + 1 + used;
    }
}

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let map = mmap(&f);
    let len = map.len();
    let base = map.as_ptr();

    let mut table = vec![Slot::empty(); TABLE_SIZE];
    let mut long: Long = HashMap::new();

    // Move a position forward to just after the next '\n'.
    let align = |pos: usize| -> usize {
        let mut p = pos;
        while p < len && map[p] != b'\n' {
            p += 1;
        }
        (p + 1).min(len)
    };

    // Split the file into STREAMS line-aligned ranges, processed in lockstep.
    let mut at = [0usize; STREAMS];
    let mut end = [len; STREAMS];
    for i in 1..STREAMS {
        at[i] = align(len / STREAMS * i);
    }
    for i in 0..STREAMS - 1 {
        end[i] = at[i + 1];
    }
    let limit = len.saturating_sub(SAFE);
    let lim = end.map(|e| e.min(limit));

    unsafe {
        // All streams advance together so their table misses overlap.
        'lockstep: loop {
            for i in 0..STREAMS {
                if at[i] >= lim[i] {
                    break 'lockstep;
                }
            }
            for i in 0..STREAMS {
                step(base, len, &mut at[i], &mut table, &mut long);
            }
        }
        // Finish whichever streams still have fast-path lines left.
        for i in 0..STREAMS {
            while at[i] < lim[i] {
                step(base, len, &mut at[i], &mut table, &mut long);
            }
        }
    }

    // Bounds-checked tail for each stream.
    for i in 0..STREAMS {
        while at[i] < end[i] {
            let rest = &map[at[i]..];
            let nl = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
            let line = &rest[..nl];
            at[i] += nl + 1;
            if line.is_empty() {
                continue;
            }
            let (temp, semi) = parse_temp_and_semi(line);
            record(&mut table, &mut long, &line[..semi], temp);
        }
    }

    // Collect + sort + print.
    let mut out: Vec<(Vec<u8>, Stats)> = Vec::new();
    for s in table.iter().filter(|s| s.len != 0) {
        out.push((s.key.to_array()[..s.len as usize].to_vec(), s.st));
    }
    out.extend(long.into_iter());
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