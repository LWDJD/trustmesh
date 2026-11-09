use trustmesh::fullsim::{Sim, SimConfig};

fn run(beta: f64, attack: f64) -> Sim {
    let cfg = SimConfig { n_initial: 200, days: 30, attack_ratio: attack, beta, ..Default::default() };
    let mut sim = Sim::new(cfg);
    sim.run();
    sim
}

fn main() {
    println!("{:<8} {:<8} {:>12} {:>12} {:>10} {:>10} {:>10}",
        "beta", "attack", "honest(rated)", "sybil(rated)", "sybil/honest", "win_rate", "all_mean");
    for &beta in &[0.3f64, 0.7] {
        for &attack in &[0.0f64, 0.1] {
            let sim = run(beta, attack);
            let n = sim.accounts.len();
            let th = 0.2f64;
            let mut sybils: Vec<usize> = Vec::new();
            let mut honests: Vec<usize> = Vec::new();
            for a in sim.accounts.iter() {
                let d = format!("{:?}", a.kind);
                if d.starts_with("Sybil") { sybils.push(a.id); }
                else if d.starts_with("Honest") { honests.push(a.id); }
            }
            let (mut hs, mut hc, mut ss, mut sc) = (0.0f64, 0usize, 0.0f64, 0usize);
            let mut win = 0usize; let mut win_n = 0usize;
            let mut gsum = 0.0; let mut gcnt = 0usize;
            for s in 0..n {
                for u in 0..n { gsum += sim.trust[s][u]; gcnt += 1; }
                let mut hm = 0.0; let mut hN = 0usize;
                let mut sm = 0.0; let mut sN = 0usize;
                for &v in &honests {
                    if v < n && sim.accounts[s].ratings[v] > th { hm += sim.trust[s][v]; hN += 1; hs += sim.trust[s][v]; hc += 1; }
                }
                for &v in &sybils {
                    if v < n && sim.accounts[s].ratings[v] > th { sm += sim.trust[s][v]; sN += 1; ss += sim.trust[s][v]; sc += 1; }
                }
                if hN >= 3 && sN >= 1 { win_n += 1; if hm / hN as f64 > sm / sN as f64 { win += 1; } }
            }
            let h_mean = if hc > 0 { hs / hc as f64 } else { 0.0 };
            let s_mean = if sc > 0 { ss / sc as f64 } else { 0.0 };
            let ratio = if h_mean > 0.0 { s_mean / h_mean } else { f64::NAN };
            let wr = if win_n > 0 { win as f64 / win_n as f64 * 100.0 } else { f64::NAN };
            println!("{:<8.1} {:<8.1} {:>12.6} {:>12.6} {:>10.3} {:>9.1}% {:>10.6}",
                beta, attack, h_mean, s_mean, ratio, wr, gsum / gcnt as f64);
        }
    }
}
