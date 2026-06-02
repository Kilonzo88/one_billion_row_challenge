#![feature(portable_simd)]  // Must be at the top of the file (crate level)

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::simd::{cmp::SimdPartialEq, u8x64};


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

fn parse_temp(temperature: &[u8]) -> i16 {
    let mut temp: i16 = 0;
    let mut mul: i16 = 1;
    for &i in temperature.iter().rev() {
        match i {
            b'.' => continue,
            b'-' => { temp = -temp; break; }
            _ => {
                temp += (i - b'0') as i16 * mul;
                mul *= 10;
            }
        }
    }
    temp
}

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let map = mmap(&f);
    let mut stats = HashMap::<Vec<u8>, (i16, i32, usize, i16)>::new();

    let mut at = 0;
    loop {
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
        let line = if next_newline.is_null() {
            rest
        } else {
            let len = (next_newline as *const u8 as usize) - (rest.as_ptr() as usize);
            &rest[..len]
        };

        at += line.len() + 1; // adds one because the first character of the newline is ignored since that marks the end of the pointer

        if line.is_empty() {
            break;
        }

        // Use libc::memchr for semicolon — same SIMD approach as newline scanning
        // Temperature is always 3-5 bytes — semicolon is always within 6 bytes from right
        let semi_pos = line.len() - 1 - line.iter().rev().take(6)
            .position(|&b| b == b';')
            .unwrap();
            
        let station = &line[..semi_pos];
        let temperature = &line[semi_pos + 1..];

        let temp = parse_temp(temperature);

        let entry = if let Some(entry) = stats.get_mut(station) {
            entry
        } else {
            stats.entry(station.to_vec()).or_insert((i16::MAX, 0, 0, i16::MIN))
        };
        entry.0 = entry.0.min(temp);
        entry.1 += temp as i32;
        entry.2 += 1;
        entry.3 = entry.3.max(temp);
    }

    let mut sorted: Vec<(String, (i16, i32, usize, i16))> = stats
        .into_iter()
        .map(|(k, v)| (unsafe { std::st`````````````````````````````````````````````````````r::from_utf8_unchecked(&k).to_string() }, v))
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    print!("{{");
    let mut iter = sorted.into_iter().peekable(); //Perfomance bottlenecks
    while let Some((station, (min, sum, count, max))) = iter.next() {
        print!(
            "{station}={:.1}/{:.1}/{:.1}",
            min as f64 / 10.0,
            (sum as f64 / count as f64) / 10.0,
            max as f64 / 10.0,
        );
        if iter.peek().is_some() {
            print!(", ");
        }
    }
    println!("}}");
}