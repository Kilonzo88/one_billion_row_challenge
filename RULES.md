# The One Billion Row Challenge (Rust Edition)

This is a local adaptation of the [One Billion Row Challenge](https://github.com/gunnarmorling/1brc), originally designed for Java. The goal is to process a text file containing 1,000,000,000 measurements and calculate the min, max, and average temperature per station as quickly as possible.

## Rules and Limits

- **No External Dependencies**: For the challenge implementation, you must use **only the Rust Standard Library (`std`)**. External crates (e.g., `memmap2`, `rayon`, `crossbeam`, `ahash`) are not permitted in the solver.
- **Single Source File**: The implementation must be provided as a single source file (`src/main.rs`).
- **Runtime Computation**: All computation must happen at application runtime. You cannot process the measurements file at build time (e.g., using `const` evaluation or build scripts to bake results into the binary).
- **Input Data Constraints**:
    - **Station Name**: Non-null UTF-8 string, min length 1 character, max length 100 bytes. Does not contain `;` or `\n`.
    - **Temperature**: Between -99.9 and 99.9 inclusive, always provided with exactly one fractional digit (e.g., `34.1`, `-5.0`).
    - **Unique Stations**: There is a maximum of **10,000** unique station names.
    - **Line Endings**: Every line ends with a `\n` character on all platforms.
- **Data Independence**: The implementation must not rely on specifics of a given data set (e.g., specific station names or data distributions).
- **Rounding Rules**: Rounding of output values must follow IEEE 754 rounding-direction **"roundTowardPositive"**.
- **Output Format**: The output must be sorted alphabetically and printed as:
  `{Abha=-23.0/18.0/59.2, Abidjan=-16.2/26.0/67.3, ...}`

## Project Structure
- `src/bin/gen.rs`: The data generator (crates allowed for generation).
- `src/main.rs`: Your challenge solution (strictly `std` only).

## Input File
The input file is `measurements.txt`, containing 1,000,000,000 rows (~13GB).
