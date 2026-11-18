//! A12 体积与耗时对照。
//!
//! 运行：`cargo run --release --bin a12_perf`
//!
//! 状态构造：n 个账户，每个账户发布一份 10 条的观点；观察者给**全部 n 个**账户
//! 打正分，于是它有 n 份来源观点。这是 `trust_distribution` 的最坏情形
//! （n × n 次查询），也是审计里 n = 800 一次 181 ms 的那个量级。

use std::time::Instant;

use tm_engine::{Engine, EngineConfig};
use tm_mincore::{Config as CoreConfig, Node, SparseView};
use tm_view::{CompressedView, ViewParams};

fn key(idx: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..8].copy_from_slice(&(idx as u64).to_le_bytes());
    k[8] = 0xA1;
    k
}

fn main() {
    let params = ViewParams::default();
    let cap = 512usize;

    println!("## 体积（每份观点的序列化字节数）");
    let eng_bytes = CompressedView::new(params).bytes();
    println!("| 实现 | 字节 | 相对 Engine |");
    println!("|---|---|---|");
    println!("| Engine（2048×5 f32） | {eng_bytes} | 100% |");
    for c in [128usize, 256, 512] {
        let b = SparseView::new(c).bytes();
        println!(
            "| mincore cap={c} | {b} | {:.1}% |",
            b as f64 / eng_bytes as f64 * 100.0
        );
    }

    println!("\n## 耗时（µs，中位数）");
    println!("| n | 操作 | Engine | mincore | 加速 |");
    println!("|---|---|---|---|---|");

    for n in [100usize, 200, 400, 800, 1600] {
        // 每个账户的观点：10 条各 0.1。
        let mut eng_views: Vec<CompressedView> = Vec::with_capacity(n);
        let mut core_views: Vec<SparseView> = Vec::with_capacity(n);
        let mut core_bytes: Vec<Vec<u8>> = Vec::with_capacity(n);
        for i in 0..n {
            let pairs: Vec<([u8; 32], f32)> =
                (1..=10).map(|j| (key((i + j) % n), 0.1f32)).collect();
            eng_views.push(CompressedView::from_pairs(params, &pairs));
            let cpairs: Vec<(u32, f32)> =
                (1..=10).map(|j| (((i + j) % n) as u32, 0.1f32)).collect();
            let v = SparseView::from_scores(cap, &cpairs);
            core_bytes.push(v.encode());
            core_views.push(v);
        }

        // —— Engine：观察者给全部 n 个账户打分 ——
        let mut e = Engine::with_seed(EngineConfig::default(), 1);
        for i in 0..n {
            e.intern(key(i));
        }
        for i in 0..n {
            e.rate(i as u32, 0.5, 0);
        }
        // 先把 n-1 份观点装进去，留一份用来计时「接收」
        //
        // 用 enumerate().skip(1) 而不是 for i in 1..n：此刻 eng_views 的长度恰好是 n，
        // 索引 1..n 与 skip(1) 之后剩余的元素是**同一个集合、同一顺序**，
        // 调用序列逐条完全相同；只是把「按下标取元素」换成迭代器，
        // 从而消掉 clippy::needless_range_loop（见下方第二条循环同样的处理）。
        for (i, v) in eng_views.iter().enumerate().skip(1) {
            e.on_view(i as u32, v.clone(), 0, 0);
        }
        let reps = 50;
        let t0 = Instant::now();
        for r in 0..reps {
            e.on_view(0, eng_views[0].clone(), 0, 0);
            let _ = r;
        }
        let eng_on_view = t0.elapsed().as_secs_f64() * 1e6 / reps as f64;

        // —— mincore ——
        let mut c = Node::new(
            CoreConfig {
                cap,
                ..CoreConfig::default()
            },
            1,
        );
        for i in 0..n {
            c.rate(i as u32, 0.5, 0);
        }
        for (i, v) in core_views.iter().enumerate().skip(1) {
            c.on_view(i as u32, v.clone(), 0, 0);
        }
        let t0 = Instant::now();
        for _ in 0..reps {
            let v = SparseView::decode(&core_bytes[0], cap).unwrap();
            c.on_view(0, v, 0, 0);
        }
        let core_on_view = t0.elapsed().as_secs_f64() * 1e6 / reps as f64;

        println!(
            "| {n} | on_view（解码+校验+入库） | {eng_on_view:.1} | {core_on_view:.1} | {:.0}× |",
            eng_on_view / core_on_view.max(1e-9)
        );

        // —— 单账户查询 ——
        let reps = 50;
        let t0 = Instant::now();
        let mut acc = 0.0f32;
        for r in 0..reps {
            acc += e.query((r % n) as u32, 0);
        }
        let eng_q = t0.elapsed().as_secs_f64() * 1e6 / reps as f64;
        let t0 = Instant::now();
        for r in 0..reps {
            acc += c.query((r % n) as u32, 0);
        }
        let core_q = t0.elapsed().as_secs_f64() * 1e6 / reps as f64;
        println!(
            "| {n} | 单账户 query | {eng_q:.1} | {core_q:.1} | {:.0}× |",
            eng_q / core_q.max(1e-9)
        );
        std::hint::black_box(acc);

        // —— 一次全量分布 ——
        let reps = if n >= 800 { 3 } else { 5 };
        let t0 = Instant::now();
        for _ in 0..reps {
            std::hint::black_box(e.trust_distribution(0));
        }
        let eng_d = t0.elapsed().as_secs_f64() * 1e3 / reps as f64;
        let t0 = Instant::now();
        for _ in 0..reps {
            std::hint::black_box(c.distribution(0));
        }
        let core_d = t0.elapsed().as_secs_f64() * 1e3 / reps as f64;
        println!(
            "| {n} | 一次全量分布 | {eng_d:.2} ms | {core_d:.2} ms | {:.0}× |",
            eng_d / core_d.max(1e-9)
        );
    }

    // 体积随 n 的总量（n = 1600 时全网的视图总量）
    println!("\n## 全网视图总量（n 份观点，每个账户一份）");
    println!("| n | Engine | mincore cap=128 | cap=256 | cap=512 |");
    println!("|---|---|---|---|---|");
    for n in [100usize, 400, 1600] {
        println!(
            "| {n} | {} KB | {} KB | {} KB | {} KB |",
            40960 * n / 1024,
            SparseView::new(128).bytes() * n / 1024,
            SparseView::new(256).bytes() * n / 1024,
            SparseView::new(512).bytes() * n / 1024
        );
    }
}
