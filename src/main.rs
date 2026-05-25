use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader},
};

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let f = BufReader::new(f);
    let mut stats = HashMap::<Vec<u8>, (f64, f64, usize, f64)>::with_capacity(512);

    for line in f.split(b'\n') {
        let line = line.unwrap();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.rsplitn(2, |&c| c == b';'); //Take this line of bytes and split it into at most 2 pieces, starting from the right, using ; as the delimiter.
        let temperature = fields.next().unwrap();
        let station = fields.next().unwrap();
        let temperature: f64 = unsafe { std::str::from_utf8_unchecked(temperature) } //skips utf-8 validation because according to the rules, the input is guaranteed to be valid utf-8
            .parse()
            .unwrap();
        let entry = if let Some(entry) = stats.get_mut(station) {
            entry
        } else {
            stats
                .entry(station.to_vec())
                .or_insert((f64::MAX, 0., 0, f64::MIN))
        };
        entry.0 = entry.0.min(temperature); //min value
        entry.1 += temperature; //sum of all values
        entry.2 += 1; //count of all values
        entry.3 = entry.3.max(temperature); //max value
    }

    print!("{{");
    let mut stats: Vec<_> = stats
        .into_iter()
        .map(|(k, v)| (String::from_utf8(k).unwrap(), v))
        .collect(); //convert the hashmap into a vector of tuples
    stats.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut stats = stats.into_iter().peekable();
    while let Some((station, (min, sum, count, max))) = stats.next() {
        print!("{station}={min}/{}/{max}", sum / (count as f64));
        if stats.peek().is_some() {
            print!(", ");
        }
    }
    println!("}}");
}
