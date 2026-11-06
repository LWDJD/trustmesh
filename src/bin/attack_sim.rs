//! 攻击模拟报告：女巫攻击与信任潜伏。

use social_trust::attack::{latent_attack, sybil_attack};

fn main() {
    println!("攻击模拟报告");
    println!("========================================\n");

    let honest_n = 200;
    let target = 7;
    let beta = 0.7;

    // ---- 女巫攻击：被骗关注比例 ----
    println!("[1] 女巫攻击：诚实节点「被骗关注」女巫的比例");
    println!("    诚实节点 {}, 女巫 20 个, β={}", honest_n, beta);
    println!("    注：以自己为中心的模型中，只有关注了女巫的人才会受影响");
    println!(
        "{:<10} {:<12} {:<12} {:<10} {:<12}",
        "被骗比例", "攻击前", "攻击后", "提升", "女巫均分"
    );

    for &ratio in &[0.0, 0.02, 0.05, 0.10, 0.20, 0.50] {
        let r = sybil_attack(honest_n, 20, 2, ratio, target, beta, 2026);
        println!(
            "{:<10.2} {:<12.4} {:<12.4} {:<10.2} {:<12.4}",
            ratio, r.target_before, r.target_after, r.lift, r.sybil_mean
        );
    }

    // ---- 女巫攻击：规模 ----
    println!("\n[2] 女巫攻击：女巫数量（被骗比例锁 10%）");
    println!(
        "{:<10} {:<12} {:<12} {:<10} {:<12}",
        "女巫数", "攻击前", "攻击后", "提升", "女巫均分"
    );

    for &sybil_n in &[10usize, 20, 50, 100] {
        let r = sybil_attack(honest_n, sybil_n, 2, 0.10, target, beta, 2027);
        println!(
            "{:<10} {:<12.4} {:<12.4} {:<10.2} {:<12.4}",
            sybil_n, r.target_before, r.target_after, r.lift, r.sybil_mean
        );
    }

    // ---- β 对女巫攻击的影响 ----
    println!("\n[3] 个人权重 β 对女巫攻击的影响（女巫 20 个，被骗 10%）");
    println!(
        "{:<8} {:<12} {:<12} {:<10} {:<12}",
        "β", "攻击前", "攻击后", "提升", "女巫均分"
    );

    for &b in &[0.3, 0.5, 0.7, 0.9] {
        let r = sybil_attack(honest_n, 20, 2, 0.10, target, b, 2028);
        println!(
            "{:<8.1} {:<12.4} {:<12.4} {:<10.2} {:<12.4}",
            b, r.target_before, r.target_after, r.lift, r.sybil_mean
        );
    }

    // ---- 潜伏攻击 ----
    println!("\n[4] 信任潜伏攻击：潜伏者数量（评分 0.5 突变到 1.0, β={}）", beta);
    println!(
        "{:<10} {:<12} {:<12} {:<10} {:<12}",
        "潜伏者数", "诚实时", "突变后", "提升", "潜伏者均分"
    );

    for &agent_n in &[10usize, 20, 40, 80] {
        let r = latent_attack(200, agent_n, target, 0.5, 1.0, beta, 3333);
        println!(
            "{:<10} {:<12.4} {:<12.4} {:<10.2} {:<12.4}",
            agent_n, r.target_honest, r.target_attack, r.lift, r.agent_score
        );
    }

    // ---- β 对潜伏攻击的影响 ----
    println!("\n[5] 个人权重 β 对潜伏攻击的影响（潜伏者 40 个）");
    println!(
        "{:<8} {:<12} {:<12} {:<10}",
        "β", "诚实时", "突变后", "提升"
    );

    for &b in &[0.3, 0.5, 0.7, 0.9] {
        let r = latent_attack(200, 40, target, 0.5, 1.0, b, 3334);
        println!(
            "{:<8.1} {:<12.4} {:<12.4} {:<10.2}",
            b, r.target_honest, r.target_attack, r.lift
        );
    }

    // ---- 两种攻击效率对比 ----
    println!("\n[6] 女巫 vs 潜伏：相同节点数下的效率（女巫被骗 100%，最坏情况）");
    println!(
        "{:<10} {:<16} {:<16} {:<12}",
        "节点数", "女巫提升", "潜伏提升", "倍数"
    );

    for &k in &[20usize, 50, 100] {
        let s = sybil_attack(200, k, 2, 1.0, target, beta, 4444);
        let l = latent_attack(200, k, target, 0.5, 1.0, beta, 4445);
        let ratio = if l.lift.abs() > 1e-9 {
            s.lift / l.lift
        } else {
            f64::INFINITY
        };
        println!(
            "{:<10} {:<16.2} {:<16.2} {:<12.2}",
            k, s.lift, l.lift, ratio
        );
    }

    println!("\n完成。");
}
