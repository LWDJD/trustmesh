//! tm_exp 层级的持久化验收：**应用层视角**的存盘 → 读回 → 恢复。
//!
//! 与 crates/engine/src/snapshot.rs 内部的 17 条单测不同，这里刻意不复用引擎
//! 内部辅助函数，而是像真正的应用层那样：
//!
//! 1. 只用公开 API（snapshot / encode / decode / restore）把状态搬进字节；
//! 2. 字节**真的经过一次 Vec 容器打包/拆包**（模拟写文件/读文件，但不碰磁盘：
//!    测试不该依赖仓库目录可写）；
//! 3. 逐位比对 query 与 shadow 分布，然后让两条网络**继续跑**，确认后续轨迹也相同。
//!
//! 这些断言钉住的是「库对外承诺」而不是「内部实现细节」，所以引擎侧改实现
//! 也不会误报。

use tm_engine::agg::AggMode;
use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig, EngineSnapshot};

const N: usize = 24;

fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k[8] = 0xA5;
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams {
            buckets: 128,
            hashers: 5,
            seed: 31,
        },
        agg_mode: AggMode::Mean,
        ..Default::default()
    }
}

fn engine_seed(i: usize) -> u64 {
    (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5EED_0000_0000_0001
}

/// 建一套小网络并跑满 3 轮，返回 (节点, 最后一轮的 now)。
fn network(rounds: u64) -> (Vec<Engine>, u64) {
    let ecfg = cfg();
    let mut nodes: Vec<Engine> = (0..N)
        .map(|i| {
            let mut e = Engine::with_seed(ecfg.clone(), engine_seed(i));
            for j in 0..N {
                e.intern(key(j));
            }
            for k in 0..8 {
                let j = (i + 3 * k) % N;
                e.rate(j as u32, 1.0 + ((i + j) % 4) as f32 * 0.5, 0);
            }
            e
        })
        .collect();
    let mut now = 0;
    for round in 0..rounds {
        now = advance(&mut nodes, round);
    }
    (nodes, now)
}

fn advance(nodes: &mut [Engine], round: u64) -> u64 {
    let now = round * 20;
    let views: Vec<CompressedView> = nodes.iter().map(|e| e.export_view(now)).collect();
    for i in 0..nodes.len() {
        let reqs = nodes[i].tick(now, 6);
        for r in reqs {
            let src = r.idx as usize;
            nodes[i].on_view(r.idx, views[src].clone(), now, now);
        }
    }
    now
}

/// 模拟应用层「存盘」：把每台引擎的快照编码后塞进一个 Vec 容器。
fn pack(nodes: &[Engine], now: u64) -> Vec<u8> {
    let mut blob = Vec::new();
    blob.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
    blob.extend_from_slice(&now.to_le_bytes());
    for e in nodes {
        let bytes = e.snapshot(now).encode();
        blob.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        blob.extend_from_slice(&bytes);
    }
    blob
}

/// 模拟应用层「读盘」：拆容器、decode、restore。任何一步失败都返回 None。
fn unpack(blob: &[u8]) -> Option<(u64, Vec<Engine>)> {
    let count = u32::from_le_bytes([*blob.first()?, *blob.get(1)?, *blob.get(2)?, *blob.get(3)?]);
    let now = u64::from_le_bytes([
        *blob.get(4)?,
        *blob.get(5)?,
        *blob.get(6)?,
        *blob.get(7)?,
        *blob.get(8)?,
        *blob.get(9)?,
        *blob.get(10)?,
        *blob.get(11)?,
    ]);
    let mut pos = 12usize;
    let mut engines = Vec::new();
    for _ in 0..count {
        let len = u32::from_le_bytes([
            *blob.get(pos)?,
            *blob.get(pos + 1)?,
            *blob.get(pos + 2)?,
            *blob.get(pos + 3)?,
        ]) as usize;
        pos += 4;
        let end = pos.checked_add(len)?;
        engines.push(Engine::restore(
            cfg(),
            EngineSnapshot::decode(blob.get(pos..end)?)?,
        )?);
        pos = end;
    }
    if pos != blob.len() {
        return None;
    }
    Some((now, engines))
}

/// 一个 f32 位模式的汉明距离（0 = 逐位相同）。
fn bit_gap(a: f32, b: f32) -> u32 {
    (a.to_bits() ^ b.to_bits()).count_ones()
}

// ---------------------------------------------------------------------------
// ① 200 万字节级的状态也能离线搬一遍（这里用小网络，断言同样的性质）
// ---------------------------------------------------------------------------
#[test]
fn restored_network_is_bit_identical() {
    let (nodes, now) = network(3);
    let blob = pack(&nodes, now);
    let (snap_now, restored) = unpack(&blob).expect("应用层打包/拆包必须能往返");
    assert_eq!(snap_now, now);
    assert_eq!(restored.len(), N);

    let mut points = 0usize;
    let mut max_bits = 0u32;
    for i in 0..N {
        for t in [0u64, now / 2, now, now * 100, now * 10_000] {
            for target in 0..N as u32 {
                let a = nodes[i].query(target, t);
                let b = restored[i].query(target, t);
                max_bits = max_bits.max(bit_gap(a, b));
                points += 1;
            }
        }
        assert_eq!(
            nodes[i].stats(),
            restored[i].stats(),
            "节点 {i} 的统计计数不同"
        );
        assert_eq!(
            nodes[i].round_remaining(),
            restored[i].round_remaining(),
            "节点 {i} 的轮次进度不同"
        );
        assert_eq!(
            nodes[i].shares(),
            restored[i].shares(),
            "节点 {i} 的份额表不同"
        );
    }
    assert_eq!(points, N * 5 * N);
    assert_eq!(max_bits, 0, "query 必须逐位相同（实际最大位差 {max_bits}）");
}

// ---------------------------------------------------------------------------
// ② shadow 分布（对外可见结果）逐位相同
// ---------------------------------------------------------------------------
#[test]
fn shadow_distribution_is_identical_after_restore() {
    let (nodes, now) = network(3);
    let blob = pack(&nodes, now);
    let (_, restored) = unpack(&blob).expect("必须能往返");
    for i in 0..N {
        let a = nodes[i].trust_distribution(now);
        let b = restored[i].trust_distribution(now);
        assert_eq!(a.len(), b.len(), "节点 {i} 的分布长度不同");
        for ((ai, av), (bi, bv)) in a.iter().zip(b.iter()) {
            assert_eq!(*ai, *bi, "节点 {i} 的分布顺序不同");
            assert_eq!(
                av.to_bits(),
                bv.to_bits(),
                "节点 {i} 对 {ai} 的份额位模式不同"
            );
        }
        // 总和也必须逐位相同（不是「差得很少」）。
        let sa: f32 = a.iter().map(|(_, v)| *v).sum();
        let sb: f32 = b.iter().map(|(_, v)| *v).sum();
        assert_eq!(sa.to_bits(), sb.to_bits());
    }
}

// ---------------------------------------------------------------------------
// ③ 恢复后的网络继续跑，必须与原件同轨迹（抽样器 + 随机流都装回去了）
// ---------------------------------------------------------------------------
#[test]
fn restored_network_continues_on_same_trajectory() {
    let (mut nodes, now) = network(3);
    let blob = pack(&nodes, now);
    let (_, mut restored) = unpack(&blob).expect("必须能往返");

    for round in 3..6u64 {
        let t_orig = advance(&mut nodes, round);
        let t_rest = advance(&mut restored, round);
        assert_eq!(t_orig, t_rest);
        for i in 0..N {
            assert_eq!(
                nodes[i].stats(),
                restored[i].stats(),
                "第 {round} 轮节点 {i} 的抽样次数分叉"
            );
            assert_eq!(
                nodes[i].round_remaining(),
                restored[i].round_remaining(),
                "第 {round} 轮节点 {i} 的轮次进度分叉"
            );
            for target in 0..N as u32 {
                let a = nodes[i].query(target, t_orig);
                let b = restored[i].query(target, t_orig);
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "第 {round} 轮节点 {i} 对 {target} 的 query 位模式不同"
                );
            }
            assert_eq!(
                nodes[i].trust_distribution(t_orig),
                restored[i].trust_distribution(t_orig),
                "第 {round} 轮节点 {i} 的 shadow 分布不同"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ④ 应用层的「配置对不上」用例：参数变了必须拒绝，而不是悄悄跑歪
// ---------------------------------------------------------------------------
#[test]
fn wrong_config_is_rejected_at_the_application_boundary() {
    let (nodes, now) = network(2);
    let snaps: Vec<EngineSnapshot> = nodes.iter().map(|e| e.snapshot(now)).collect();
    let mut wrong = cfg();
    wrong.view_params = ViewParams {
        buckets: 128,
        hashers: 5,
        seed: 32, // 只差 seed：字节长度完全一样，最容易被漏掉的一种
    };
    for snap in snaps {
        assert!(
            Engine::restore(wrong.clone(), snap).is_none(),
            "应用层传错参数必须被拒（库不能悄悄用对方的桶布局）"
        );
    }
}
