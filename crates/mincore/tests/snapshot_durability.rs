//! 状态快照的**集成**断言（从 crate 外部看 Node::snapshot / Node::restore）。
//!
//! 单元测试在 src/snapshot.rs 里逐条覆盖编解码与防御；这个文件只钉
//! **外部可见的承诺**，也就是调用方真正关心的那三条：
//!
//! 1. **重启后逐位回到关机前**：同一个节点，snapshot -> encode -> decode -> restore
//!    之后，分布 / 逐账户查询 / 下一次抽样全部与「没重启」**逐位相同**；
//! 2. **快照可以落盘再读回**：真实写一份文件、读回来、恢复，行为仍逐位相同；
//! 3. **坏负载一律 None**：截断、掺假、配置不符都不会 panic，也不会返回半个节点。
//!
//! 为什么单独一个文件：tests/e2e_asserts.rs 是 A12 的 S1~S6 端到端判据
//! （跑整张仿真图，秒级），而这里全是 O(state) 的构造性检查，混进去会让那个
//! 文件既慢又难读；tests/resource_limits.rs 则是 max_views / max_list 的专项。

use tm_mincore::{Config, Node, NodeSnapshot, SparseView};

// ══════════════════════════ 小工具 ══════════════════════════

/// 逐位可比较的分布：`(账户, 声望位模式)` + `mass` 的位模式（-0.0 与 0.0 不同）。
fn bits(n: &Node, now: u64) -> (Vec<(u32, u32)>, u64) {
    let d = n.distribution(now);
    (
        d.entries.iter().map(|&(a, v)| (a, v.to_bits())).collect(),
        d.mass.to_bits(),
    )
}

/// 逐账户查询的位模式。
fn queries(n: &Node, now: u64, accounts: &[u32]) -> Vec<u32> {
    accounts
        .iter()
        .map(|&a| n.query(a, now).to_bits())
        .collect()
}

/// 走完整链路：快照 -> 编码 -> 解码 -> 恢复。
fn reboot(n: &Node, cfg: Config) -> (Vec<u8>, Node) {
    let bytes = n.snapshot().encode();
    let snap = NodeSnapshot::decode(&bytes, cfg).expect("解码必须成功");
    let node = Node::restore(snap, cfg).expect("恢复必须成功");
    (bytes, node)
}

/// 造一个「用过一阵子」的节点：清单、六份 cap 混用的观点、半轮抽样、连败、统计。
fn used_node(cfg: Config, seed: u64) -> Node {
    let mut n = Node::new(cfg, seed);
    for i in 1..=12u32 {
        n.rate(i, 0.05 * f32::from(i as u16), u64::from(i) * 7);
    }
    // 撤销一个不存在的账户（不该产生任何副作用）。
    n.rate(999, 0.0, 0);
    let caps = [512usize, 64, 128, 8, 8, 256];
    for (k, owner) in [1u32, 2, 3, 4, 5, 6].iter().enumerate() {
        let pairs = [(200u32 + k as u32, 1.0f32), (300, 0.6), (400, 0.2)];
        assert!(n.on_view(
            *owner,
            SparseView::from_scores(caps[k], &pairs),
            10 + k as u64,
            20 + k as u64
        ));
    }
    assert_eq!(n.tick(3).len(), 3, "六个候选抽三个");
    n.on_view_failed(2);
    n.on_view_failed(2);
    assert!(
        !n.on_view(7, SparseView::new(8), 0, 0),
        "owner 没有正分，必须被拒"
    );
    n
}

// ══════════════════════════ 1. 重启后逐位回到关机前 ══════════════════════════

/// 分布逐位相同（含 `mass` 的位模式：空分布是 -0.0，不能与 0.0 混为一谈）。
#[test]
fn i01_重启后分布逐位相同() {
    let cfg = Config::default();
    let a = used_node(cfg, 7);
    let (_, b) = reboot(&a, cfg);
    for now in [0u64, 40, 1000, 100_000] {
        assert_eq!(bits(&a, now), bits(&b, now), "now = {now} 时分布分叉");
    }
}

/// 逐账户查询逐位相同。
#[test]
fn i02_重启后逐账户查询逐位相同() {
    let cfg = Config::default();
    let a = used_node(cfg, 7);
    let (_, b) = reboot(&a, cfg);
    let accounts: Vec<u32> = (0..420).collect();
    for now in [0u64, 40, 1000] {
        assert_eq!(
            queries(&a, now, &accounts),
            queries(&b, now, &accounts),
            "now = {now} 时 query 分叉"
        );
    }
}

/// **判决性用例**：恢复后的每一次抽样都与「没重启」逐次相同。
///
/// 这条同时守住了位图的五个字段：只存 order + remaining 会让恢复后的
/// sync() 判定「未同步」并重建 + reset()，本轮进度被抹掉、抽样立刻分叉。
#[test]
fn i03_重启后连续抽样逐次相同() {
    let cfg = Config::default();
    let mut a = used_node(cfg, 7);
    let (_, mut b) = reboot(&a, cfg);
    for round in 0..20 {
        let ra = a.tick(2);
        let rb = b.tick(2);
        assert_eq!(ra, rb, "第 {round} 次 tick 分叉");
        assert_eq!(
            a.round_remaining(),
            b.round_remaining(),
            "第 {round} 次 tick 后剩余数不同"
        );
    }
    assert_eq!(a.probe(), b.probe(), "probe 分叉");
}

/// 恢复后连败计数接着之前的值往上加（而不是从 0 重来）。
#[test]
fn i04_重启后连败计数接着涨() {
    let cfg = Config::default();
    let a = used_node(cfg, 7);
    let (_, mut b) = reboot(&a, cfg);
    assert_eq!(a.claims().fail_streak(2), 2);
    assert_eq!(b.claims().fail_streak(2), 2, "连败计数必须原样恢复");
    b.on_view_failed(2);
    assert_eq!(b.claims().fail_streak(2), 3, "恢复后必须接着涨");
}

/// 恢复后的节点仍是**可用**的：继续 rate / on_view，行为与没重启的节点逐位一致。
#[test]
fn i05_重启后继续操作仍逐位一致() {
    let cfg = Config::default();
    let mut a = used_node(cfg, 7);
    let (_, mut b) = reboot(&a, cfg);
    for i in 20..30u32 {
        a.rate(i, 0.4, 500);
        b.rate(i, 0.4, 500);
        let v = SparseView::from_scores(64, &[(i + 1, 0.5), (i + 2, 0.5)]);
        assert_eq!(a.on_view(i, v.clone(), 500, 500), b.on_view(i, v, 500, 500));
    }
    assert_eq!(bits(&a, 1000), bits(&b, 1000), "恢复后继续操作导致分布分叉");
    assert_eq!(a.stats(), b.stats());
    assert_eq!(a.round_remaining(), b.round_remaining());
}

// ══════════════════════════ 2. 落盘再读回 ══════════════════════════

/// 真实写一份文件到临时目录、读回来、恢复：行为仍逐位相同。
#[test]
fn i06_快照落盘再读回行为不变() {
    let cfg = Config::default();
    let mut a = used_node(cfg, 7);
    let path = std::env::temp_dir().join(format!("tm_mincore_snapshot_{}.bin", std::process::id()));
    {
        let bytes = a.snapshot().encode();
        std::fs::write(&path, &bytes).expect("写快照文件");
    }
    let loaded = std::fs::read(&path).expect("读快照文件");
    let _ = std::fs::remove_file(&path);
    let snap = NodeSnapshot::decode(&loaded, cfg).expect("落盘再读回必须能解");
    let mut b = Node::restore(snap, cfg).expect("落盘再读回必须能恢复");
    assert_eq!(a.tick(3), b.tick(3), "落盘往返后抽样分叉");
    assert_eq!(bits(&a, 100), bits(&b, 100), "落盘往返后分布分叉");
}

/// 字节数不随调用次数漂移：同一份状态编码两次必须完全相同。
#[test]
fn i07_编码确定性() {
    let cfg = Config::default();
    let a = used_node(cfg, 7);
    let b1 = a.snapshot().encode();
    let b2 = used_node(cfg, 7).snapshot().encode();
    assert_eq!(b1, b2, "同一份状态必须编码成同一串字节");
    assert_eq!(b1, a.snapshot().encode());
    assert_eq!(
        a.snapshot().bytes(),
        b1.len(),
        "bytes() 必须等于 encode().len()"
    );
}

// ══════════════════════════ 3. 坏负载一律 None ══════════════════════════

/// 每一处前缀截断都不能解（且不 panic）。
#[test]
fn i08_前缀截断全部被拒() {
    let cfg = Config::default();
    let bytes = used_node(cfg, 7).snapshot().encode();
    for i in 0..bytes.len() {
        assert!(
            NodeSnapshot::decode(&bytes[..i], cfg).is_none(),
            "截断到 {i} 字节被解出来了"
        );
    }
}

/// 配置不符（任何一个旋钮）都不能恢复 —— 否则「用 512 的 cap 恢复 128 的视图」会静默发生。
#[test]
fn i09_配置不符被拒() {
    let cfg = Config::default();
    let bytes = used_node(cfg, 7).snapshot().encode();
    let variants = [
        Config { beta: 0.5, ..cfg },
        Config {
            half_life: 96,
            ..cfg
        },
        Config { cap: 256, ..cfg },
        Config {
            decay_enabled: false,
            ..cfg
        },
        Config {
            max_views: 0,
            ..cfg
        },
        Config { max_list: 0, ..cfg },
    ];
    for c in variants {
        assert!(
            NodeSnapshot::decode(&bytes, c).is_none(),
            "配置 {c:?} 不该解出快照"
        );
    }
    assert!(
        NodeSnapshot::decode(&bytes, cfg).is_some(),
        "原配置必须能解"
    );
}

/// 空节点也能完整往返：view_count == 0、抽样为空、随机数状态不变。
#[test]
fn i10_空节点往返() {
    let cfg = Config::default();
    let mut a = Node::new(cfg, 3);
    let bytes = a.snapshot().encode();
    let (_, mut b) = reboot(&a, cfg);
    assert_eq!(a.snapshot().encode(), bytes, "空节点编码必须稳定");
    assert_eq!(b.view_count(), 0);
    assert!(b.tick(5).is_empty());
    assert!(a.tick(5).is_empty());
    assert_eq!(
        a.snapshot().encode(),
        b.snapshot().encode(),
        "空节点往返后仍逐字节相同"
    );
}

/// 篡改**头部**的任意一个字节都不能解（魔数 / 版本 / 摘要三块逐字节都不放过）。
///
/// 为什么不抽查载荷区：那里有大量「合法但改语义」的字段（`last_check`、
/// `published_at`、各计数），改动它们是**允许**的 —— 快照要挡住的是结构不合法，
/// 不是「内容与原来不一样」（后者只能靠存储层的认证解决，见 snapshot.rs 的边界说明）。
#[test]
fn i11_头部单字节篡改被拒() {
    let cfg = Config::default();
    let bytes = used_node(cfg, 7).snapshot().encode();
    // 头部 = 魔数 9 + 版本 4 + 摘要 8 = 21 字节。
    let mut rejected = 0usize;
    for i in 0..21usize {
        let mut bad = bytes.clone();
        bad[i] ^= 0xFF;
        assert!(
            NodeSnapshot::decode(&bad, cfg).is_none(),
            "篡改头部第 {i} 字节居然解出来了"
        );
        rejected += 1;
    }
    assert_eq!(rejected, 21, "头部 21 个字节逐个篡改都必须被拒");
}
