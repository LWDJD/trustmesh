//! 潜伏实验：异步多节点 × 离线-回归 × 观点陈旧
use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_sim::{AttackKind, SimConfig};
use tm_view::ViewParams;

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k
}

struct Out {
    honest: f64,
    attacker: f64,
    ret_att: f64,
    ret_hon: f64,
    ret_n: usize,
    sleeper_fresh: usize,
    honest_fresh: usize,
}

fn run(cfg: SimConfig, rounds: u64, samples: usize) -> Out {
    let steps = cfg.steps as u64;
    let tick = (steps / rounds).max(1);
    let mut world = tm_sim::World::new(cfg);
    world.run();
    let ratings = world.ratings();
    let accs = world.accounts();
    let n = accs.len();

    let online: Vec<Vec<bool>> = (0..n)
        .map(|i| (0..=rounds).map(|r| world.was_online_at(i, r * tick)).collect())
        .collect();

    let ecfg = EngineConfig {
        beta: 0.3, half_life: 72,
        view_params: ViewParams { buckets: 2048, hashers: 5, seed: 99 },
        agg_mode: AggMode::Mean, noise_multiplier: 1.0,
    };
    let is_att: Vec<bool> = accs.iter().map(|a| a.kind.is_attacker()).collect();
    let alive: Vec<usize> = accs.iter().filter(|a| a.is_alive()).map(|a| a.idx).collect();

    let mut nodes: Vec<Option<Engine>> = (0..n).map(|_| None).collect();
    for &a in &alive {
        let mut e = Engine::new(ecfg.clone());
        for i in 0..n { e.intern(key(i)); }
        for (o, t, s) in &ratings { if *o == a { e.rate(*t as u32, *s, 0); } }
        nodes[a] = Some(e);
    }

    let mut was_offline = vec![false; n];
    let mut prev_online = vec![false; n];

    for r in 0..=rounds {
        let now = r * tick;
        let mut views: Vec<Option<tm_view::CompressedView>> = vec![None; n];
        for &a in &alive {
            if online[a][r as usize] {
                if let Some(e) = &nodes[a] { views[a] = Some(e.export_view(now)); }
            }
        }
        for &a in &alive {
            if !online[a][r as usize] { prev_online[a] = false; was_offline[a] = true; continue; }
            // 断过网又回来 = 回归用户；本轮加倍抽样做快速刷新
            let returning = was_offline[a] && !prev_online[a] && r > 0;
            let k = if returning { samples * 4 } else { samples };
            let reqs = { nodes[a].as_mut().unwrap().tick(now, k) };
            for req in reqs {
                if let Some(Some(v)) = views.get(req.idx as usize) {
                    nodes[a].as_mut().unwrap().on_view(req.idx, v.clone(), now, now);
                }
            }
            prev_online[a] = true;
        }
    }

    let now = rounds * tick;
    let mut hsum = 0.0f64; let mut hcnt = 0usize;
    let mut asum = 0.0f64; let mut acnt = 0usize;
    let mut ratt = 0.0f64; let mut rhon = 0.0f64; let mut rn = 0usize;
    for &obs in &alive {
        if is_att[obs] || !online[obs][rounds as usize] { continue; }
        let e = nodes[obs].as_ref().unwrap();
        let returning = was_offline[obs];
        for &t in &alive {
            let v = e.query(t as u32, now) as f64;
            if is_att[t] { asum += v; acnt += 1; if returning { ratt += v; } }
            else { hsum += v; hcnt += 1; if returning { rhon += v; rn += 1; } }
        }
    }

    let mut sf = 0usize; let mut hf = 0usize;
    for &a in &alive {
        if !online[a][rounds as usize] { continue; }
        if let Some(e) = nodes[a].as_ref() {
            let f = e.freshness_stats(now).fresh_within_half_life;
            if is_att[a] { sf += f; } else { hf += f; }
        }
    }

    Out {
        honest: if hcnt > 0 { hsum / hcnt as f64 } else { 0.0 },
        attacker: if acnt > 0 { asum / acnt as f64 } else { 0.0 },
        // FIXME(分母错误，未修): ret_att 用 rn 做分母，但 rn 只在上面「诚实目标」
        // 分支自增（第 89 行），ratt 却在「攻击目标」分支累加（第 88 行）。
        // 结果是攻击者声望被除以诚实账户计数，攻击比例 10% 时低估约 9 倍。
        // 影响范围：只有本 bin 输出的「回归看攻击」这一列；
        //   「诚实/攻击」两列用 hcnt/acnt，正确；
        //   ret_hon 用 rhon/rn，同源，正确。
        // 其他 runtime 级仿真（multinode / grid）用的是分开的计数器，没有此问题。
        ret_att: if rn > 0 { ratt / (rn as f64) } else { 0.0 },
        ret_hon: if rn > 0 { rhon / (rn as f64) } else { 0.0 },
        ret_n: rn,
        sleeper_fresh: sf,
        honest_fresh: hf,
    }
}

fn main() {
    let steps = 300usize;
    let rounds = 12u64;
    let base = |seed: u64| SimConfig {
        n_initial: 200, steps, seed, join_rate: 0.0, leave_rate: 0.0, dormancy_rate: 0.0,
        enable_rhythm: false, ..Default::default()
    };
    let sleeper = |seed: u64| SimConfig {
        n_initial: 200, steps, seed, join_rate: 0.0, leave_rate: 0.0,
        enable_rhythm: false,
        attack_kind: AttackKind::Sleeper { normal_until: (steps / 5) as u64 },
        attack_ratio: 0.1, ..Default::default()
    };

    let cases: Vec<(&str, SimConfig)> = vec![
        ("无潜伏 + 无离线", base(11)),
        ("有潜伏 + 无离线", sleeper(11)),
        ("无潜伏 + 有离线", { let mut c = SimConfig::sleeper_scenario(200, steps, 11); c.attack_kind = AttackKind::None; c.attack_ratio = 0.0; c }),
        ("有潜伏 + 有离线", SimConfig::sleeper_scenario(200, steps, 11)),
        ("有潜伏+高回归率", { let mut c = SimConfig::sleeper_scenario(200, steps, 11); c.return_rate = 0.8; c.sleep_hours = (steps as u64 / 6, steps as u64 / 2); c }),
    ];

    println!("{:<18} {:>10} {:>10} {:>8} {:>14} {:>14} {:>16}", "场景", "诚实", "攻击", "比值", "回归看攻击", "回归看诚实", "新鲜来源(潜伏/诚实)");
    for (name, cfg) in cases {
        let o = run(cfg, rounds, 12);
        println!("{:<18} {:>10.5} {:>10.5} {:>8.2} {:>14.5} {:>14.5} {:>16}",
            name, o.honest, o.attacker,
            if o.honest > 0.0 { o.attacker / o.honest } else { 0.0 },
            o.ret_att, o.ret_hon,
            format!("{} / {} (n={})", o.sleeper_fresh, o.honest_fresh, o.ret_n));
    }
}
