//! 2×2 因子实验：攻击者连接度（普通/高连接）× 攻击者数量（单个/多个）
//! 衡量指标：被引入女巫的声望 ÷ 诚实账户基线。全部开启影响力预算 + 接收方归一化。
use tm_agg::AggMode;
use tm_exp::{run_case_full, PropConfig};
use tm_sim::{AttackKind, Kind, SimConfig};

fn baseline_of(o: &tm_exp::Outcome) -> f32 {
    let mut s = 0.0f64; let mut c = 0usize;
    for &ob in &o.honest {
        if ob >= o.rep.len() { continue; }
        for &h in &o.honest { if h < o.rep[ob].len() { s += o.rep[ob][h] as f64; c += 1; } }
    }
    if c > 0 { (s / c as f64) as f32 } else { 0.0 }
}

fn sybil_mean(o: &tm_exp::Outcome, sybils: &[usize]) -> f32 {
    let mut s = 0.0f64; let mut c = 0usize;
    for &ob in &o.honest {
        if ob >= o.rep.len() { continue; }
        for &x in sybils { if x < o.rep[ob].len() { s += o.rep[ob][x] as f64; c += 1; } }
    }
    if c > 0 { (s / c as f64) as f32 } else { 0.0 }
}

fn main() {
    let base = SimConfig {
        n_initial: 400, steps: 120, seed: 7,
        follow_min: 30, follow_max: 45, max_follows: 60,
        ..Default::default()
    }.with_attack(AttackKind::Infiltrator, 0.08);

    let mut w0 = tm_sim::World::new(base.clone());
    w0.run();
    let ratings0 = w0.ratings();
    let n0 = w0.accounts().len();

    // 入度 = 有多少人给它打过分
    let mut indeg = vec![0usize; n0];
    for (_, t, _) in &ratings0 { if *t < n0 { indeg[*t] += 1; } }
    let mut order: Vec<usize> = (0..n0).collect();
    order.sort_by_key(|i| std::cmp::Reverse(indeg[*i]));

    let is_atk: Vec<bool> = w0.accounts().iter().map(|a| matches!(a.kind, Kind::Infiltrator { .. })).collect();
    let honest_only: Vec<usize> = (0..n0).filter(|i| !is_atk[*i]).collect();
    let mut honest_sorted = honest_only.clone();
    honest_sorted.sort_by_key(|i| indeg[*i]);

    // 普通节点：取入度中位附近的诚实账户；高连接：取入度最高的诚实账户
    let median_idx = honest_sorted[honest_sorted.len() / 2];
    let top: Vec<usize> = order.iter().copied().filter(|i| !is_atk[*i]).take(5).collect();
    let ordinary: Vec<usize> = honest_sorted.iter().copied()
        .filter(|i| indeg[*i] <= indeg[median_idx]).rev().take(5).collect();

    println!("网络：{} 账户，入度中位数 {}，最高 {}",
        n0, indeg[median_idx], indeg[order[0]]);
    println!("普通攻击者入度：{:?}", ordinary.iter().map(|i| indeg[*i]).collect::<Vec<_>>());
    println!("高连接攻击者入度：{:?}", top.iter().map(|i| indeg[*i]).collect::<Vec<_>>());

    let alive = w0.alive_count();
    for (conn_label, pool) in [("普通节点", &ordinary), ("高连接节点", &top)] {
        for (cnt_label, k) in [("单个", 1usize), ("多个(5)", 5)] {
            let atk: Vec<usize> = pool.iter().copied().take(k).collect();
            println!("\n══════ {} × {} ══════", conn_label, cnt_label);
            println!("{:<8} {:>12} {:>12} {:>8}", "引入数", "女巫均分", "诚实均分", "比值");
            for n_sybil in [1usize, 10, 50] {
                let mut inject = Vec::new();
                let mut sybils = Vec::new();
                for s in 0..n_sybil {
                    let sx = alive + s;
                    sybils.push(sx);
                    for &a in &atk { inject.push((a, sx, 1.0)); }
                }
                // 额外账户里包含女巫
                let pc = PropConfig {
                    beta: 0.3, samples: 16, rounds: 4, mode: AggMode::Mean,
                    use_noise_floor: true, exclude_zero: true,
                    banlist: false, ban_penalty: 0.5, ban_floor: 0.2,
                    ban_exclude: 0.0, ban_lambda: 0.0,
                    normalize_view: true, attacker_normalizes: true, receiver_normalizes: true,
                };
                let o = run_case_full(base.clone(), &pc, &inject, &sybils, &atk);
                let b = baseline_of(&o);
                let sm = sybil_mean(&o, &sybils);
                println!("{:<8} {:>12.5} {:>12.5} {:>8.2}", n_sybil, sm, b, if b > 0.0 { sm / b } else { 0.0 });
            }
        }
    }
}
