use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
};

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let f = BufReader::new(f);
    let mut stats = BTreeMap::<String, (f64, f64, usize, f64)>::new();
    for line in f.lines() {
        let line = line.unwrap();
        let (station, temperature) = line.split_once(';').unwrap();
        let temperature: f64 = temperature.parse().unwrap();
        let stats = stats
            .entry(station.to_string()) //get the entry of a given station
            .or_insert((f64::MAX, 0., 0, f64::MIN)); //if the entry is empty, insert the default value
        stats.0 = stats.0.min(temperature); // min
        stats.1 += temperature; // sum
        stats.2 += 1; // count
        stats.3 = stats.3.max(temperature); // max
    }
    print!("{{");
    let mut stats = stats.into_iter().peekable();
    while let Some((station, (min, sum, count, max))) = stats.next() {
        print!("{station}={min}/{}/{max}", sum / (count as f64));
        if stats.peek().is_some() {
            print!(", ");
        }
    }
    println!("}}");
}
