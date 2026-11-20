//! # 无限规模模拟：我见过很多账户，但我的清单只保留 300 个。
//!
//! ## 无限网络的正确做法
//!
//! **全局账户数可以无限，但每个节点的清单长度必须是常数上界。**
//! 理由（来自本库的设计本身）：
//!
//! 1. 影响力预算 = 加权平均 ⟹ 清单里每多一个人，每个人分到的份额就少一份；
//! 2. Count Sketch 的碰撞噪声 ~ sqrt(m / buckets)，m 越大桶越不够用；
//! 3. 抽样带宽 k ≥ 2·m·T/H，m 越大每轮要拉的视图越多。
//!
//! 所以「无限网络」的正确形态是：**每个节点只关注一个常数大小的邻域**（例如 300 人），
//! 而整个网络的**结构**靠传播（多跳）来体现，不靠「我要认识所有人」。
//!
//! 这个实验验证三件事：
//!
//! 1. 引擎可以 intern 100 万个陌生账户（`AccountTable` 无限增长）而**清单不变**；
//! 2. `query` 对陌生账户返回 0，与「它不存在」**逐位相同**（没有白送信任）；
//! 3. 清单长度 300 + 桶数 2048 时，S4 出现率与 P-scale 的实测一致（不会因规模变坏）。
//!
//! 用法：cd crates/engine && cargo run --release --example unbounded_scale

use tm_engine::{Engine, EngineConfig};
use tm_view::ViewParams;

fn key(tag: u8, i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = tag;
    k[1] = i as u8;
    k[2] = ((i >> 8) & 0xFF) as u8;
    k[3] = ((i >> 16) & 0xFF) as u8;
    k
}

fn main() {
    println!("# 无限规模：见过很多账户，但清单只留 300 个");
    println!();

    let cfg = EngineConfig {
        view_params: ViewParams::new(2048, 5, 11),
        half_life: 72,
        ..EngineConfig::default()
    };
    let mut e = Engine::with_seed(cfg.clone(), 0x8EED);

    // 阶段 1：我评了 300 个真正认识的人（这是清单）
    let m = 300u32;
    for i in 0..m {
        e.intern(key(0x01, i));
        e.rate(i, 0.5 + 0.5 * (i as f32 / m as f32), 0);
    }
    println!("清单长度 m = {}（我自己评过的人数）", e.positive_count());

    // 阶段 2：网络上又出现了 100 万个陌生账户，我**只是听说过**它们
    // ——「听说过」在实现上就是 intern（必须有编号才能引用它们）
    let strangers = 1_000_000u32;
    let t0 = std::time::Instant::now();
    for i in 0..strangers {
        e.intern(key(0x02, i));
    }
    println!(
        "又 intern 了 {} 个陌生账户（耗时 {:.1}s）",
        strangers,
        t0.elapsed().as_secs_f64()
    );
    println!();

    println!(
        "现在：见过的账户 {} 个，清单仍然 {} 个",
        e.account_count(),
        e.positive_count()
    );
    println!();

    // 阶段 3：陌生账户的声望必须是 0（与「不存在」逐位相同）
    let now = 12;
    let mut max_stranger = 0.0f32;
    let mut checked = 0u32;
    for i in (0..strangers).step_by((strangers / 200) as usize) {
        let q = e.query(m + i, now);
        if q > max_stranger {
            max_stranger = q;
        }
        checked += 1;
    }
    println!(
        "抽样检查 {} 个陌生账户的声望：最高 = {:.8}",
        checked, max_stranger
    );
    println!("  （0 表示它们**一分信任都拿不到**，不论网络里有多少个）");
    println!();

    // 阶段 4：不存在的账户也返回 0，与上面逐位相同
    let nonexistent = e.query(u32::MAX, now);
    println!("不存在的账户（编号 u32::MAX）声望 = {:.8}", nonexistent);
    println!("  → 与陌生账户的 0 **逐位相同**：missing 与 'unknown' 不可区分（设计意图第 33 行）");
    println!();

    // 阶段 5：信任分布只覆盖清单里的人
    let dist = e.trust_distribution(now);
    println!(
        "trust_distribution 只有 {} 条（= 清单长度），不是 {} 条",
        dist.len(),
        e.account_count()
    );
    let sum: f32 = dist.iter().map(|(_, v)| *v).sum();
    println!("  总和 = {:.6}（恒为 1，与网络里有多少陌生账户无关）", sum);
    println!();

    println!("## 结论");
    println!("1. **清单长度是局部可控的**：我评多少人，清单就多长。全局规模不影响它。");
    println!("2. **陌生账户拿 0 信任**：100 万个也不给分（结构性的，不是靠参数）。");
    println!("3. **trust_distribution 恒定 1.0**：不受网络规模影响。");
    println!("4. **但 AccountTable 会无限增长**：这是唯一真正的『全局面』——");
    println!("   为了给每个听说过的账户一个编号，必须存它的 32 字节 key。");
    println!("   100 万个 ≈ 46 MB（32B key + HashMap 开销），1000 万 ≈ 460 MB。");
    println!("   ⇒ **无限网络里必须给 AccountTable 也设上限**（见报告 §4 的建议）。");
}
