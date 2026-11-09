//! 打印不同写入规模下的噪声底与查询误差，便于挑选参数。
//!
//! 运行：cargo run --release --example noise_report

use tm_view::{AccountKey, CompressedView, ViewParams, XorShift64};

fn pairs(n: usize, seed: u64) -> Vec<(AccountKey, f32)> {
    let mut rng = XorShift64::new(seed);
    (0..n)
        .map(|_| {
            let key = rng.next_key();
            let unit = (rng.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
            (key, 0.1 + 0.9 * unit)
        })
        .collect()
}

fn main() {
    let params = ViewParams::default();
    println!(
        "params: buckets={} hashers={} seed={}",
        params.buckets, params.hashers, params.seed
    );
    println!(
        "bytes: {} （与写入量无关）",
        CompressedView::new(params).bytes()
    );
    println!();
    println!(
        "{:>8} {:>10} {:>10} {:>10} {:>10}",
        "entries", "median", "p95", "max", "mean_err"
    );

    for &n in &[0usize, 100, 1000, 10000] {
        let entries = pairs(n, 1);
        let view = CompressedView::from_pairs(params, &entries);
        let stats = view.noise_floor(2000, 42);
        let mean_error = if entries.is_empty() {
            0.0
        } else {
            entries
                .iter()
                .map(|(key, score)| (view.query(key) - *score).abs())
                .sum::<f32>()
                / entries.len() as f32
        };
        println!(
            "{:>8} {:>10.4} {:>10.4} {:>10.4} {:>10.5}",
            n, stats.median, stats.p95, stats.max, mean_error
        );
    }

    // 写入量固定时，桶数如何影响误差与体积。
    println!();
    println!("10000 条写入时，桶数对误差的影响（hashers=5）：");
    println!(
        "{:>8} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "buckets", "median", "p95", "max", "mean_err", "KiB"
    );
    let entries = pairs(10000, 1);
    for &buckets in &[1024usize, 2048, 4096, 8192] {
        let params = ViewParams::new(buckets, 5, 0);
        let view = CompressedView::from_pairs(params, &entries);
        let stats = view.noise_floor(2000, 42);
        let mean_error = entries
            .iter()
            .map(|(key, score)| (view.query(key) - *score).abs())
            .sum::<f32>()
            / entries.len() as f32;
        println!(
            "{:>8} {:>10.4} {:>10.4} {:>10.4} {:>10.5} {:>10}",
            buckets,
            stats.median,
            stats.p95,
            stats.max,
            mean_error,
            view.bytes() / 1024
        );
    }
}
