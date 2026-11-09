//! 复现性：同一个种子必须得到逐位相同的世界。
//!
//! 这是仿真最硬的约束——没有它，所有参数扫描的结论都不可信。

use tm_sim::{AttackKind, SimConfig, World};

fn 配置(seed: u64) -> SimConfig {
    SimConfig {
        n_initial: 120,
        steps: 60,
        seed,
        snapshot_every: 10,
        attack_kind: AttackKind::Sybil { cluster: 2 },
        attack_ratio: 0.1,
        ..SimConfig::default()
    }
}

fn 跑(seed: u64) -> (Vec<(usize, usize, f32)>, tm_sim::Snapshot, Vec<tm_sim::Snapshot>) {
    let mut w = World::new(配置(seed));
    w.run();
    (w.ratings(), w.report(), w.history.clone())
}

#[test]
fn 同种子两次逐位相同() {
    let a = 跑(2024);
    let b = 跑(2024);
    assert_eq!(a.0, b.0, "评分明细必须逐位相同");
    assert_eq!(a.1, b.1, "最终快照必须逐位相同");
    assert_eq!(a.2.len(), b.2.len(), "历史快照数量必须相同");
    assert_eq!(a.2, b.2, "每份历史快照都必须相同");
}

#[test]
fn 不同种子得到不同世界() {
    let a = 跑(1);
    let b = 跑(2);
    assert_ne!(a.0, b.0, "换种子后评分明细不该完全相同");
}

#[test]
fn 逐步推进与一次跑完等价() {
    let cfg = 配置(77);
    let steps = cfg.steps;
    let mut a = World::new(cfg.clone());
    a.run();

    let mut b = World::new(cfg);
    for _ in 0..steps {
        b.step();
    }
    assert_eq!(a.time, b.time);
    assert_eq!(a.step, b.step);
    assert_eq!(a.ratings(), b.ratings());
    assert_eq!(a.report(), b.report());
}

#[test]
fn 历史快照按间隔记录() {
    let mut w = World::new(配置(5));
    assert_eq!(w.history.len(), 1, "初始应有一份 t=0 快照");
    assert_eq!(w.history[0].step, 0);
    w.run();
    assert_eq!(w.history.len(), 1 + w.cfg.steps / 10);
    for (k, snap) in w.history.iter().enumerate() {
        assert_eq!(snap.step, k * 10);
        assert_eq!(snap.time, (k * 10) as u64);
    }
    // 最后一份历史快照就是当前状态
    assert_eq!(w.history.last().unwrap(), &w.report());
}

#[test]
fn 相同配置重复构造的世界一致() {
    let cfg = SimConfig {
        n_initial: 80,
        seed: 31337,
        ..SimConfig::default()
    };
    let a = World::new(cfg.clone());
    let b = World::new(cfg);
    assert_eq!(a.accounts.len(), b.accounts.len());
    for (x, y) in a.accounts.iter().zip(b.accounts.iter()) {
        assert_eq!(x.kind, y.kind);
        assert_eq!(x.community, y.community);
        assert_eq!(x.quality, y.quality);
        assert_eq!(x.activity, y.activity);
        assert_eq!(x.follows, y.follows);
        assert_eq!(x.interest, y.interest);
    }
}

#[test]
fn 关闭快照记录后没有历史() {
    let mut w = World::new(SimConfig {
        n_initial: 20,
        steps: 5,
        snapshot_every: 0,
        ..SimConfig::default()
    });
    w.run();
    assert!(w.history.is_empty());
    // report 仍然可用
    assert_eq!(w.report().step, 5);
}
