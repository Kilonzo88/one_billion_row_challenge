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
    let mut stats = HashMap::<Vec<u8>, (i16, i32, usize, i16)>::new();

    for line in map.split(|&c| c == b'\n') {
        if line.is_empty() {
            continue; // skip empty lines
        }
        let mut fields = line.rsplitn(2, |&c| c == b';');
        let (Some(temperature), Some(station)) = (fields.next(), fields.next()) else {
            panic!("bad line: {}", unsafe{std::str::from_utf8_unchecked(line)})
        };
        
        let mut temp:i16 = 0;
        let mut mul = 1;
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

        let entry = if let Some(entry) = stats.get_mut(station) {
            entry
        } else {
            stats.entry(station.to_vec()).or_insert((i16::MAX,0, 0, i16::MIN))
        };
        entry.0 = entry.0.min(temp);
        entry.1 += temp as i32;
        entry.2 += 1;
        entry.3 = entry.3.max(temp);
    }

    let mut sorted: Vec<(String, (i16, i32, usize, i16))> = stats
        .into_iter()
        .map(|(k, v)| (unsafe { std::str::from_utf8_unchecked(&k).to_string() }, v))
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    print!("{{");
    let mut iter = sorted.into_iter().peekable();
    while let Some((station, (min, sum, count, max))) = iter.next() {
        print!("{station}={:.1}/{:.1}/{:.1}", 
            (min as f64) / 10.,
            (sum as f64) / 10. / (count as f64),
            (max as f64) / 10.,
        );
        if iter.peek().is_some() {
            print!(", ");
        }
    }
    println!("}}");
}
