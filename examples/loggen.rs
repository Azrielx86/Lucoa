//! Load generator for testing lucoa: appends synthetic log lines to a file.
//!
//! cargo run --release --example loggen -- tmp.log --rate 1000
//! cargo run --release --example loggen -- big.log --total 20000000 --width 100

use clap::Parser;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Cli {
    /// File to append to (created if missing)
    path: PathBuf,

    /// Lines per second
    #[arg(long, default_value_t = 1000)]
    rate: u64,

    /// Stop after this many lines (default: run forever)
    #[arg(long)]
    total: Option<u64>,

    /// Minimum line width, padded with dots
    #[arg(long, default_value_t = 80)]
    width: usize,
}

fn make_line(seq: u64, width: usize) -> String {
    let level = match seq % 10 {
        0 => "ERROR",
        3 | 7 => "WARN",
        _ => "INFO",
    };
    let mut s = format!("{seq:010} [{level}] job-{:03} processed request", seq % 500);
    if s.len() < width {
        s.extend(std::iter::repeat_n('.', width - s.len()));
    }
    s
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cli.path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);

    let tick = Duration::from_millis(20);
    let per_tick = (cli.rate / 50).max(1);
    let start = Instant::now();
    let mut written = 0u64;

    loop {
        let tick_start = Instant::now();
        for _ in 0..per_tick {
            if let Some(total) = cli.total
                && written >= total
            {
                w.flush()?;
                eprintln!(
                    "wrote {written} lines in {:.1}s",
                    start.elapsed().as_secs_f64()
                );
                return Ok(());
            }
            writeln!(w, "{}", make_line(written, cli.width))?;
            written += 1;
        }
        w.flush()?;
        if written % 1_000_000 < per_tick {
            eprintln!("{written} lines...");
        }
        let elapsed = tick_start.elapsed();
        if elapsed < tick {
            std::thread::sleep(tick - elapsed);
        }
    }
}
