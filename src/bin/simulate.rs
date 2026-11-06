//! 模拟验证：生成不同结构的图，对比精确解与近似解，输出统计报告。

use std::time::Instant;

use social_trust::exact::{residual, solve_exact};
use social_trust::local_push::{solve_local, LocalPushParams};
use social_trust::rng::Rng;
use social_trust::sim;

fn main() {
    println!("社交信任计算：数学验证报告");
    println!("========================================\n");

    let beta = 0.7;

    println!("参数：β={}（个人权重），群体权重={}\n", beta, 1.0 - beta);

    // ---- 第一部分：精确解与近似解的一致性 ----
    println!("[1] 精确解 vs 近似解（不同图结构）");
    println!("{:<8} {:<6} {:<8} {:<12} {:<12} {:<10}", "图", "N", "边数", "精确解耗时", "近似解耗时", "最大误差");

    for &(name, kind) in &[("BA", 0), ("WS", 1), ("ER", 2)] {
        for &n in &[30usize, 60, 100] {
            let mut rng = Rng::new(1234 + n as u64);
            let mut g = match kind {
                0 => sim::gen_ba(n, 3, &mut rng),
                1 => sim::gen_ws(n, 6, 0.1, &mut rng),
                _ => sim::gen_er(n, 4, &mut rng),
            };
            sim::random_ratings(&mut g, 6, &mut rng);

            let edges: usize = (0..n).map(|u| g.out_degree(u)).sum();

            let t0 = Instant::now();
            let exact = solve_exact(&g, beta);
            let t_exact = t0.elapsed();

            let params = LocalPushParams {
                beta,
                eps: 1e-4,
                max_iter: 500_000,
            };
            let t1 = Instant::now();
            let mut approx = vec![vec![0.0; n]; n];
            // 只对前 20 个观察者跑近似解，控制总时间
            for s in 0..n.min(20) {
                approx[s] = solve_local(&g, s, &params);
            }
            let t_approx = t1.elapsed();

            let mut max_err = 0.0f64;
            for s in 0..n.min(20) {
                for u in 0..n {
                    let d = (approx[s][u] - exact[s][u]).abs();
                    if d > max_err {
                        max_err = d;
                    }
                }
            }

            println!(
                "{:<8} {:<6} {:<8} {:<12.2?} {:<12.2?} {:<10.2e}",
                name, n, edges, t_exact, t_approx, max_err
            );
        }
    }

    // ---- 第二部分：不动点残差 ----
    println!("\n[2] 不动点残差（越小越接近理论解）");
    println!("{:<8} {:<6} {:<16} {:<16}", "图", "N", "精确解残差", "近似解残差");

    for &(name, kind) in &[("BA", 0), ("WS", 1)] {
        let n = 60;
        let mut rng = Rng::new(777);
        let mut g = match kind {
            0 => sim::gen_ba(n, 3, &mut rng),
            _ => sim::gen_ws(n, 6, 0.1, &mut rng),
        };
        sim::random_ratings(&mut g, 6, &mut rng);

        let exact = solve_exact(&g, beta);
        let r_exact = residual(&g, &exact, beta);

        let params = LocalPushParams {
            beta,
            eps: 1e-4,
            max_iter: 500_000,
        };
        let mut approx = vec![vec![0.0; n]; n];
        for s in 0..n {
            approx[s] = solve_local(&g, s, &params);
        }
        let r_approx = residual(&g, &approx, beta);

        println!("{:<8} {:<6} {:<16.2e} {:<16.2e}", name, n, r_exact, r_approx);
    }

    // ---- 第三部分：收敛性 ----
    println!("\n[3] 精度 eps 与误差的关系（BA, N=60）");
    println!("{:<10} {:<14} {:<12}", "eps", "最大误差", "耗时");

    let n = 60;
    let mut rng = Rng::new(999);
    let mut g = sim::gen_ba(n, 3, &mut rng);
    sim::random_ratings(&mut g, 6, &mut rng);
    let exact = solve_exact(&g, beta);

    for &eps in &[1e-2, 1e-3, 1e-4, 1e-5, 1e-6] {
        let params = LocalPushParams {
            beta,
            eps,
            max_iter: 2_000_000,
        };
        let t0 = Instant::now();
        let pi = solve_local(&g, 5, &params);
        let dt = t0.elapsed();
        let err = (0..n)
            .map(|u| (pi[u] - exact[5][u]).abs())
            .fold(0.0f64, f64::max);
        println!("{:<10.0e} {:<14.2e} {:<12.2?}", eps, err, dt);
    }

    // ---- 第四部分：β 的影响 ----
    println!("\n[4] 个人权重 β 对结果分布的影响（BA, N=60）");
    println!("{:<8} {:<12} {:<12} {:<12}", "β", "均值", "标准差", "极差");

    for &b in &[0.3, 0.5, 0.7, 0.9] {
        let mut rng = Rng::new(555);
        let mut g = sim::gen_ba(n, 3, &mut rng);
        sim::random_ratings(&mut g, 6, &mut rng);

        let pi = solve_exact(&g, b);
        let mut vals = Vec::new();
        for s in 0..n {
            for u in 0..n {
                vals.push(pi[s][u]);
            }
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        let std = var.sqrt();
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

        println!("{:<8.1} {:<12.4} {:<12.4} {:<12.4}", b, mean, std, max - min);
    }

    // ---- 第五部分：规模扩展 ----
    println!("\n[5] 近似解的规模扩展（BA, 每个观察者）");
    println!("{:<8} {:<10} {:<14} {:<14}", "N", "边数", "单次耗时", "残差质量");

    for &n in &[100usize, 500, 1000, 2000] {
        let mut rng = Rng::new(4242);
        let mut g = sim::gen_ba(n, 3, &mut rng);
        sim::random_ratings(&mut g, 8, &mut rng);

        let params = LocalPushParams {
            beta,
            eps: 1e-4,
            max_iter: 500_000,
        };

        let t0 = Instant::now();
        let pi = solve_local(&g, 0, &params);
        let dt = t0.elapsed();

        let mass: f64 = pi.iter().sum();
        let edges: usize = (0..n).map(|u| g.out_degree(u)).sum();

        println!("{:<8} {:<10} {:<14.2?} {:<14.4}", n, edges, dt, mass);
    }

    // ---- 第六部分：抽样估计（范围化）----
    println!("\n[6] 抽样估计：用 k 个样本代替全网（BA, N=200）");
    println!("{:<8} {:<14} {:<14} {:<12}", "k", "RMSE", "95%误差", "理论 1/√k");

    let n = 200;
    let mut rng = Rng::new(31337);
    let mut g = sim::gen_ba(n, 3, &mut rng);
    sim::random_ratings(&mut g, 8, &mut rng);

    let params = LocalPushParams {
        beta,
        eps: 1e-4,
        max_iter: 500_000,
    };
    let pi = solve_local(&g, 0, &params);
    let true_mean = pi.iter().sum::<f64>() / n as f64;

    for &k in &[10usize, 40, 160, 640] {
        let trials = 200;
        let mut errs = Vec::with_capacity(trials);
        for _ in 0..trials {
            let mut sum = 0.0;
            for _ in 0..k {
                sum += pi[rng.next_usize(n)];
            }
            let est = sum / k as f64;
            errs.push((est - true_mean).abs());
        }
        let mse = errs.iter().map(|e| e * e).sum::<f64>() / trials as f64;
        let rmse = mse.sqrt();
        println!(
            "{:<8} {:<14.4e} {:<14.4e} {:<12.4e}",
            k,
            rmse,
            1.96 * rmse,
            1.0 / (k as f64).sqrt()
        );
    }

    println!("\n完成。");
}
