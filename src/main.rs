use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;

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

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let map = mmap(&f);
    let mut stats = HashMap::<Vec<u8>, (f64, f64, usize, f64)>::new();

    for line in map.split(|&c| c == b'\n') {
        if line.is_empty() {
            continue; // skip empty lines
        }
        let mut fields = line.rsplitn(2, |&c| c == b';');
        let temperature = fields.next().unwrap();
        let station = fields.next().unwrap();
        let temperature: f64 = unsafe { std::str::from_utf8_unchecked(temperature) }
            .parse()
            .unwrap();
        let entry = if let Some(entry) = stats.get_mut(station) {
            entry
        } else {
            stats
                .entry(station.to_vec())
                .or_insert((f64::MAX, 0., 0, f64::MIN))
        };
        entry.0 = entry.0.min(temperature);
        entry.1 += temperature;
        entry.2 += 1;
        entry.3 = entry.3.max(temperature);
    }

    let mut sorted: Vec<(String, (f64, f64, usize, f64))> = stats
        .into_iter()
        .map(|(k, v)| (unsafe { String::from_utf8_unchecked(k) }, v))
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    print!("{{");
    let mut iter = sorted.into_iter().peekable();
    while let Some((station, (min, sum, count, max))) = iter.next() {
        print!("{station}={min:.1}/{:.1}/{max:.1}", sum / (count as f64));
        if iter.peek().is_some() {
            print!(", ");
        }
    }
    println!("}}");
}
