//! # 多跳传播的累积误差：cap 截断在每一跳丢一点，传播 N 跳后丢多少？
//!
//! 作者的表述：「误差取决于信息密度，**信息能传多远**，也取决于每个用户设置的
//! 数据大小上限。**文件越大精度越高**。」
//!
//! 这提示一个必须量化的问题：**截断误差会不会随传播距离累积？**
//!
//! A12 的 `publish()` 在每一跳都做：算出完整分布 → 截断到 cap 条 → 归一化。
//! 所以每一跳都丢一点尾巴。传播 N 跳之后：
//!
//! * 如果是**加性累积**：N 跳丢 N × 单跳损失 ⇒ 传播距离受严重限制；
//! * 如果是**收敛**（截断只丢本来就很小的值）：误差有上界，传播可以很远。
//!
//! 这个探针用**真引擎**模拟一条链：A → B → C → D → ...，每跳都截断到 cap，
//! 测量「原始分布」与「N 跳后分布」的差异。
//!
//! 用法：cd crates/engine && cargo run --release --example propagation_error

use tm_engine::view::ViewParams;
use tm_engine::{Engine, EngineConfig};

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = i as u8;
    k[1] = ((i >> 8) & 0xFF) as u8;
    k[2] = 0xC1;
    k
}

fn main() {
    println!("# 多跳传播：截断误差会累积吗？");
    println!();

    println!("注：现行引擎用 Count Sketch（2048×5 桶），没有 cap 截断。");
    println!("所以这个探针测的是**传播本身的衰减累积**（衰减 + 每跳重算导致的平滑）。");
    println!();

    let cfg = EngineConfig {
        view_params: ViewParams::new(2048, 5, 13),
        half_life: 72,
        ..EngineConfig::default()
    };

    // 源节点 S：给 100 个账户打分（其中 10 个高分、90 个低分）
    let n_src_targets = 100u32;
    let mut src = Engine::with_seed(cfg.clone(), 0x5001);
    for i in 0..n_src_targets {
        src.intern(key(i));
        // 前 10 个高分，其余低分（制造长尾）
        let score = if i < 10 { 1.0 } else { 0.02 };
        src.rate(i, score, 0);
    }

    // 记录源的真值分布（这是「理想情况」）
    let truth: Vec<f32> = (0..n_src_targets).map(|i| src.query(i, 0)).collect();
    println!(
        "源节点的分布：前 10 个账户合计 {:.6}，其余 90 个合计 {:.6}",
        truth[..10].iter().sum::<f32>(),
        truth[10..].iter().sum::<f32>()
    );
    println!();

    // 链式传播：S → N1 → N2 → ... 每跳一个新节点收上一跳的观点 + 自己也评分
    println!("跳数  |  前10账户合计  |  尾部90账户合计  |  总质量  |  与源的最大偏差");

    let mut prev = src.export_view(0);
    for hop in 1..=10u32 {
        let now = hop as u64 * 12;
        // 新节点：**只认识部分账户**（模拟真实的异质网络）
        // 前 5 个它自己评过（个人项），其余 95 个它完全没评过（只能靠传播）
        let mut node = Engine::with_seed(cfg.clone(), 0x5000 + hop as u64);
        for i in 0..n_src_targets {
            node.intern(key(i));
        }
        for i in 0..5u32 {
            node.rate(i, 1.0, 0);
        }
        // 收到上一跳的观点（owner = 一个它评过的人，用编号 0 占位）
        node.on_view(0, prev.clone(), now, now);
        // 发布自己的
        prev = node.export_view(now);

        // 测量上一跳分布
        let d: Vec<f32> = (0..n_src_targets).map(|i| node.query(i, now)).collect();
        let head: f32 = d[..10].iter().sum();
        let tail: f32 = d[10..].iter().sum();
        let total: f32 = d.iter().sum();
        let maxdiff = d
            .iter()
            .zip(truth.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        println!(
            "{:>4}  |  {:>12.6}  |  {:>13.6}  |  {:>6.4}  |  {:>12.8}",
            hop, head, tail, total, maxdiff
        );
    }

    println!();
    println!("## 读法");
    println!("* 如果「最大偏差」随跳数**单调增长** ⟹ 误差累积，传播距离受限；");
    println!("* 如果它**收敛到一个上界** ⟹ 误差有界，信息可以传很远。");
    println!();
    println!("这个探针用的是 Count Sketch（无 cap 截断），所以它测的是**传播本身的平滑**。");
    println!(
        "换成 A12 的定长表后，还要**额外**叠加每跳的 cap 截断损失（见docs/audit/A12-kernel.md §5.2）。"
    );
}
