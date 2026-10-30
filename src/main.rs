use std::fs::File;
use std::os::unix::io::AsRawFd;

const TABLE_BITS: u32 = 15; // 32768 slots (2 MB); the 1BRC limit is ~10k stations
const TABLE_SIZE: usize = 1 << TABLE_BITS;
const MASK: usize = TABLE_SIZE - 1;

/// One slot == one cache line. The first 16 bytes of the name live inline as two
/// u64s (zero padded), so most lookups compare two integers and never leave the line.
/// Names longer than 16 bytes compare their remainder against the first occurrence,
/// which is still sitting in the mmap, so names up to 100 bytes need no extra storage.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Slot {
    k0: u64,
    k1: u64,
    name: &'static [u8], // full name, borrowed from the mmap
    len: u32,            // 0 == empty slot
    min: i16,
    max: i16,
    sum: i64,
    count: u32,
}

impl Slot {
    fn empty() -> Self {
        Slot { k0: 0, k1: 0, name: &[], len: 0, min: i16::MAX, max: i16::MIN, sum: 0, count: 0 }
    }
}

#[inline(always)]
fn mmap(f: &File) -> &'static [u8] {
    let len = f.metadata().unwrap().len(); //Asks the OS "how big is this file?"
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
/// (unchanged from your version)
#[inline(always)]
fn parse_temp_and_semi(line: &[u8]) -> (i16, usize) {
    let n = line.len();

    let b0 = line[n - 1] - b'0'; // tenths
    let b1 = line[n - 3] - b'0'; // ones
    let mut temp = b0 as i16 + b1 as i16 * 10;

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

/// First 16 bytes of the name as two little-endian u64s, zero padded past `len`.
/// `can_read16` says whether 16 bytes starting at the name are inside the mapping.
#[inline(always)]
fn prefix(station: &[u8], can_read16: bool) -> (u64, u64) {
    let len = station.len();
    let v: u128 = if can_read16 {
        unsafe { std::ptr::read_unaligned(station.as_ptr() as *const u128) }
    } else {
        // only the last few lines of the file end up here
        let mut b = [0u8; 16];
        let n = len.min(16);
        b[..n].copy_from_slice(&station[..n]);
        u128::from_le_bytes(b)
    };
    let v = if len < 16 { v & ((1u128 << (len * 8)) - 1) } else { v };
    (v as u64, (v >> 64) as u64)
}

/// Returns the index of the slot for this station, inserting it if new.
#[inline(always)]
fn find(table: &mut [Slot], station: &'static [u8], can_read16: bool) -> usize {
    let len = station.len();
    let (k0, k1) = prefix(station, can_read16);

    // one multiply; top bits are the best mixed
    let mut idx = ((k0 ^ k1.rotate_left(32)).wrapping_mul(0x9e3779b97f4a7c15) >> (64 - TABLE_BITS)) as usize;

    loop {
        let s = unsafe { table.get_unchecked_mut(idx) };

        if s.len as usize == len && s.k0 == k0 && s.k1 == k1 && (len <= 16 || s.name[16..] == station[16..]) {
            return idx;
        }
        if s.len == 0 {
            s.k0 = k0;
            s.k1 = k1;
            s.name = station;
            s.len = len as u32;
            return idx;
        }
        idx = (idx + 1) & MASK;
    }
}

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let map = mmap(&f);
    let mut table = vec![Slot::empty(); TABLE_SIZE];

    let mut at = 0;
    while at < map.len() {
        let line_start = at;
        let rest = &map[at..];
        // Search through rest.len() bytes starting at rest.as_ptr() for a newline byte
        let next_newline = unsafe {
            libc::memchr(
                rest.as_ptr() as *const libc::c_void,
                b'\n' as libc::c_int,
                rest.len(),
            )
        };

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

        let station: &'static [u8] = &line[..semi_pos];
        let can_read16 = line_start + 16 <= map.len();

        let idx = find(&mut table, station, can_read16);
        let s = unsafe { table.get_unchecked_mut(idx) };
        s.min = s.min.min(temp);
        s.max = s.max.max(temp);
        s.sum += temp as i64;
        s.count += 1;
    }

    let mut sorted: Vec<(String, i16, i64, u32, i16)> = table
        .iter()
        .filter(|s| s.len != 0)
        .map(|s| {
            (
                unsafe { std::str::from_utf8_unchecked(s.name).to_string() },
                s.min,
                s.sum,
                s.count,
                s.max,
            )
        })
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    print!("{{");
    let mut iter = sorted.into_iter().peekable();
    while let Some((station, min, sum, count, max)) = iter.next() {
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