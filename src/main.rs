#![feature(portable_simd)] // Must be at the top of the file (crate level)

use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::os::unix::io::AsRawFd;
use std::simd::{cmp::SimdPartialEq, u8x64};

use std::borrow::Borrow;

#[derive(Clone, Debug)]
struct StationKey {
    len: usize,
    bytes: [u8; 100], // stations are <= to 100 bytes in length and it's better for them to be in an array for cache optimization
}

impl PartialEq for StationKey {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && &self.bytes[..self.len] == &other.bytes[..other.len]
        //compares only valid prefix and removes the trailing '0'
    }
}

impl Eq for StationKey {}

impl std::hash::Hash for StationKey {
    #[inline(always)]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes[..self.len].hash(state);
    }
}

impl StationKey {
    #[inline(always)]
    fn new(slice: &[u8]) -> Self {
        let mut bytes = [0u8; 100];
        bytes[..slice.len()].copy_from_slice(slice);
        Self {
            len: slice.len(),
            bytes,
        }
    }
}

impl Borrow<[u8]> for StationKey {
    #[inline(always)]
    fn borrow(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[derive(Default)]
struct StationHasher(u64);

impl Hasher for StationHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        let mut i = 0;
        while i + 8 <= bytes.len() {
            //This new while loop is just one CPU per iteration
            let val = u64::from_ne_bytes(bytes[i..i + 8].try_into().unwrap()); //Reads all 8 bytes at once as a single memory load into a u64 CPU register. Because we already check the bounds of each slice in the 'while' statement, doing the unsafe'unwrap_unchecked doesn't yield a perfomance gain because the compiler automatically skips checking'
            hash ^= val.wrapping_mul(0x9e3779b97f4a7c15);
            hash = hash.rotate_left(31);
            i += 8;
        }
        // handle remaining bytes
        if i < bytes.len() {
            let mut val = 0u64;
            for &b in &bytes[i..] {
                val = (val << 8) | b as u64;
            }
            hash ^= val.wrapping_mul(0x9e3779b97f4a7c15);
            hash = hash.rotate_left(31);
        }
        self.0 = hash;
    }
}

#[inline(always)]
fn mmap(f: &File) -> &'static [u8] {
    let len = f.metadata().unwrap().len(); //Asks the OS "how big is this file?" — gets back 13 billion something bytes. We need this because mmap needs to know how much virtual address space to reserve
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(), // let OS choose the virtual address
            len as libc::size_t,  // how much address space to reserve
            libc::PROT_READ,      // we only need to read
            libc::MAP_SHARED,     // share the memory with other processes
            f.as_raw_fd(),        // file descriptor
            0,                    // offset meaning start from byte 0
        );
        if ptr == libc::MAP_FAILED {
            panic!("{:?}", std::io::Error::last_os_error());
        }
        std::slice::from_raw_parts(ptr as *const u8, len as usize)
    }
}

/// Parses the temperature and locates the semicolon from the **end** of a raw line.
///
/// The 1BRC temperature format is always one of these four patterns,
/// with exactly one decimal digit (i.e. one digit after the dot):
///
/// ```
/// Pattern      Example      Indices from end (len = line length)
/// -------      -------      ------------------------------------
///  X.X         5.3          [len-1]='3'  [len-2]='.'  [len-3]='5'  [len-4]=';'
/// -X.X        -5.3          [len-1]='3'  [len-2]='.'  [len-3]='5'  [len-4]='-'  [len-5]=';'
///  XX.X        53.2         [len-1]='2'  [len-2]='.'  [len-3]='3'  [len-4]='5'  [len-5]=';'
/// -XX.X       -53.2         [len-1]='2'  [len-2]='.'  [len-3]='3'  [len-4]='5'  [len-5]='-'  [len-6]=';'
/// ```
///
/// Returns `(temp, semi_pos)` where `temp` is the temperature scaled by 10
/// (e.g. `53` represents `5.3°C`) and `semi_pos` is the byte index of `';'`.
#[inline(always)]
fn parse_temp_and_semi(line: &[u8]) -> (i16, usize) {
    let n = line.len();

    // b0: always the tenths digit  (last byte,        e.g. '3' in "5.3")
    // b1: always the ones digit    (skip '.' at n-2,  e.g. '5' in "5.3")
    // [n-2] is always '.', so we never read it.
    let b0 = line[n - 1] - b'0'; // tenths
    let b1 = line[n - 3] - b'0'; // ones
    let mut temp = b0 as i16 + b1 as i16 * 10;

    // b2 is the character one position before the ones digit.
    // Its value tells us which of the four patterns we are in:
    let b2 = line[n - 4];

    let semi_pos = if b2 == b';' {
        // Pattern "X.X"  — single positive digit, semicolon is right here.
        n - 4
    } else if b2 == b'-' {
        // Pattern "-X.X" — single negative digit, negate and step back one more.
        temp = -temp;
        n - 5
    } else {
        // b2 is a digit → two-digit temperature ("XX.X" or "-XX.X").
        // Incorporate the tens-of-real-value digit (×100 in scaled units).
        temp += (b2 - b'0') as i16 * 100;

        if line[n - 5] == b';' {
            // Pattern "XX.X"  — two-digit positive, semicolon here.
            n - 5
        } else {
            // Pattern "-XX.X" — two-digit negative, negate and step back one more.
            temp = -temp;
            n - 6
        }
    };

    (temp, semi_pos)
}

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let map = mmap(&f);
    let mut stats = HashMap::<StationKey, (i16, i32, usize, i16), BuildHasherDefault<StationHasher>>::with_capacity_and_hasher(10_000, BuildHasherDefault::default());

    let mut at = 0;
    while at < map.len() {
        let rest = &map[at..];
        // Search through rest.len() bytes starting at rest.as_ptr() for a newline byte using libc's SIMD-optimized search, and return a pointer to where it was found
        let next_newline = unsafe {
            libc::memchr(
                rest.as_ptr() as *const libc::c_void, //returns a C pointer that's why it's unsafe
                b'\n' as libc::c_int,
                rest.len(),
            )
        }; //Scans for new lines faster because unlike the previous closure which called every byte, SIMD scans 32 bytes simultaneously

        //Slices each line using the pointer from memchr and creates a slice
        let (line, found_newline) = if next_newline.is_null() {
            (rest, false)
        } else {
            let len = (next_newline as *const u8 as usize) - (rest.as_ptr() as usize);
            (&rest[..len], true)
        };

        at += line.len() + if found_newline { 1 } else { 0 }; // only add one if newline was found

        if line.is_empty() {
            break;
        }

        let (temp, semi_pos) = parse_temp_and_semi(line);

        let station = &line[..semi_pos];

        let entry = if let Some(entry) = stats.get_mut(station) {
            entry
        } else {
            stats
                .entry(StationKey::new(station))
                .or_insert((i16::MAX, 0, 0, i16::MIN))
        };
        entry.0 = entry.0.min(temp);
        entry.1 += temp as i32;
        entry.2 += 1;
        entry.3 = entry.3.max(temp);
    }

    let mut sorted: Vec<(String, (i16, i32, usize, i16))> = stats
        .into_iter()
        .map(|(k, v)| {
            (
                unsafe { std::str::from_utf8_unchecked(&k.bytes[..k.len]).to_string() },
                v,
            )
        })
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    print!("{{");
    let mut iter = sorted.into_iter().peekable(); //Perfomance bottlenecks
    while let Some((station, (min, sum, count, max))) = iter.next() {
        print!(
            "{station}={:.1}/{:.1}/{:.1}",
            min as f64 / 10.0,
            (sum as f64 / count as f64) / 10.0,
            (max as f64 / 10.0),
        );
        if iter.peek().is_some() {
            print!(", ");
        }
    }
    println!("}}");
}
