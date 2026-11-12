//! 真实多节点运行时 × 最终归一化：每节点一个 Engine，输出总和恒为 1 的信任分布。
use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, SimConfig};
use tm_view::ViewParams;

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

fn run(kind: AttackKind, name: &str) {
    let cfg = SimConfig {
        n_initial: 200, steps: 100, seed: 7,
        follow_min: 20, follow_max: 30, max_follows: 40,
        ..Default::default()
    }.with_attack(kind.clone(), if matches!(kind, AttackKind::None) { 0.0 } else { 0.08 });

    let mut world = tm_sim::World::new(cfg);
    world.run();
    let ratings = world.ratings();
    let accs = world.accounts();
    let n = accs.len();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();
    let is_att: Vec<bool> = accs.iter().map(|a| a.kind.is_attacker()).collect();

    let ecfg = EngineConfig {
        beta: 0.3, half_life: 72,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean, noise_multiplier: 1.0,
        decay_enabled: true,
        decay_floor: 0.0,
        coverage_exponent: 1.0,
        remove_when_coverage: 0.0,
        group_half_life: 0,
        unreachable_after: 0,
        min_group_sources: 0,
        ..Default::default()
    };
    let mut nodes: Vec<Option<Engine>> = (0..n).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::new(ecfg.clone());
        for i in 0..n { e.intern(key(i)); }
        for (o, t, s) in &ratings { if *o == a { e.rate(*t as u32, *s, 0); } }
        nodes[a] = Some(e);
    }

    let k = 12usize;
    let rounds = 4usize;
    for r in 0..rounds {
        let mut views: Vec<Option<tm_view::CompressedView>> = vec![None; n];
        for &a in &alive { if let Some(e) = &nodes[a] { views[a] = Some(e.export_view(r as u64)); } }
        for &a in &alive {
            let reqs = { nodes[a].as_mut().unwrap().tick(r as u64, k) };
            for req in reqs {
                if let Some(Some(v)) = views.get(req.idx as usize) {
                    nodes[a].as_mut().unwrap().on_view(req.idx, v.clone(), r as u64, r as u64);
                }
            }
        }
    }

    // 用最终归一化分布统计
    let now = rounds as u64;
    let (mut hs, mut hc, mut asum, mut ac) = (0.0f64, 0usize, 0.0f64, 0usize);
    let mut sums_ok = 0usize; let mut obs_cnt = 0usize;
    for &obs in &alive {
        if is_att[obs] { continue; }
        let e = nodes[obs].as_ref().unwrap();
        let dist = e.trust_distribution(now);
        let total: f32 = dist.iter().map(|(_, v)| *v).sum();
        obs_cnt += 1;
        if dist.is_empty() || (total - 1.0).abs() < 1e-3 { sums_ok += 1; }
        for (idx, v) in dist {
            if is_att[idx as usize] { asum += v as f64; ac += 1; } else { hs += v as f64; hc += 1; }
        }
    }
    let h = if hc > 0 { hs / hc as f64 } else { 0.0 };
    let at = if ac > 0 { asum / ac as f64 } else { 0.0 };
    println!("{:<12} 诚实 {:.6}  攻击 {:.6}  比值 {:>6.3}  ｜ 总和=1 的观察者 {}/{}",
        name, h, at, if h > 0.0 { at / h } else { 0.0 }, sums_ok, obs_cnt);
}

fn main() {
    let sw = std::time::Instant::now();
    println!("真实多节点运行时（每节点一个 Engine，4 轮交换，最终归一化）\n");
    run(AttackKind::None, "基线");
    run(AttackKind::Sybil { cluster: 3 }, "女巫 x3");
    run(AttackKind::Sleeper { normal_until: 50 }, "潜伏 50h");
    run(AttackKind::Infiltrator, "渗透");
    run(AttackKind::Endorser { ratio: 0.2 }, "背书 20%");
    run(AttackKind::Spammer, "垃圾");
    println!("\n总耗时 {:.1}s", sw.elapsed().as_secs_f64());
}
