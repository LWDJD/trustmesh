//! # p_persist —— P4 端到端演示：引擎状态「存盘 → 读回 → 恢复 → 逐位一致」
//!
//! 这是**应用层**的演示程序（所以它可以用 std 的文件 I/O；库的 src 里没有也不会
//! 有 std::fs —— 序列化只产字节，读写由应用层负责）。
//!
//! 流程：
//!
//! 1. 建 200 个账户的网络，每个账户一台独立的 [tm_engine::Engine]；
//! 2. 跑 4 轮观点传播（导出 → 抽样 → 回填），让观点缓存、评分、轮次进度、
//!    随机流都处在**非平凡**的位置；
//! 3. 把每台引擎的状态拍成 [tm_engine::EngineSnapshot]，编码后写进
//!    `<仓库根>/.tmp/p_persist_network.bin`（真实落盘、再真实读回）；
//! 4. 用读回来的字节 decode + restore 出一整套**全新引擎**（原引擎不动，
//!    等价于「进程重启」）；
//! 5. 逐位对比：query 的位模式、shadow 分布，以及**继续跑 2 轮**之后
//!    两条网络是否还逐位同轨迹。
//!
//! 验收口径：
//!
//! * `最大位差` = max over 全部对比点 |a.to_bits() ^ b.to_bits()| 的 popcount
//!   （0 等价于「位模式完全相同」，不是「差得很小」）；
//! * `shadow 分布` = 每个观察者对外可见的信任分布，即
//!   [tm_engine::Engine::trust_distribution] 的结果。它是引擎内部状态的「影子」：
//!   引擎重启后，外界唯一能观测到的就是它，所以它必须逐位不变。
//!
//! ## 参数说明
//!
//! 演示把观点压缩参数调小（256 桶 × 5 哈希 = 5 KiB/份）纯粹是为了让 200 个节点
//! 跑得动；快照格式**与参数无关**（参数指纹只用于校验），换成默认的
//! 2048 × 5 也一样逐位恢复，只是文件大一倍（每份观点 40 KiB）。

use std::fs;
use std::path::{Path, PathBuf};

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig, EngineSnapshot};
use tm_store::Rng;
use tm_view::ViewParams;

/// 网络规模（账户数 = 节点数）。
const N: usize = 200;
/// 传播轮数。
const ROUNDS: u64 = 4;
/// 每轮每个节点抽样数（< 该节点的正分清单长度，所以轮次进度能留到下一轮）。
const SAMPLES: usize = 8;
/// 一轮代表多少小时（与半衰期 72 同一单位）。
const ROUND_HOURS: u64 = 20;
/// 每个节点给多少个来源打正分。
const FRIENDS: usize = 40;
/// 演示用的视图参数（见文件头「参数说明」）。
const BUCKETS: usize = 256;
const HASHERS: usize = 5;
const VIEW_SEED: u64 = 99;

/// 由账户号合成一个 32 字节公钥（演示用，与其它 bin 的口径一致）。
fn key(i: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0..8].copy_from_slice(&(i as u64).to_le_bytes());
    k[8] = 0xA5;
    k
}

/// 引擎配置：除视图参数外全部取默认值（β = 0.3、H = 72、Mean 聚合、惩罚强度 1.0）。
fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams {
            buckets: BUCKETS,
            hashers: HASHERS,
            seed: VIEW_SEED,
        },
        agg_mode: AggMode::Mean,
        ..Default::default()
    }
}

/// 每个节点的抽样随机流种子：**互不相同**。
///
/// 不给每个节点不同种子的话，200 台引擎会共享同一条抽样流，抽样结果完全同步，
/// 演示就退化成「200 个同构副本」了（真实网络不该这样）。
fn engine_seed(i: usize) -> u64 {
    (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xC0FF_EE00_0000_0001
}

/// 建一套 200 节点的网络（每个节点一台引擎，都 intern 了全部 200 个账户）。
///
/// 评分用确定性公式生成（不引入额外随机性）：节点 i 关注编号
/// (i + 7k) mod N 的 40 个来源，分数 ∈ {1.0, 1.25, …, 2.0} —— 份额不均匀，
/// 这样个人项、群体项、衰减三条路径都会被走到。
fn build_network() -> Vec<Engine> {
    let ecfg = cfg();
    let mut nodes = Vec::with_capacity(N);
    for i in 0..N {
        let mut e = Engine::with_seed(ecfg.clone(), engine_seed(i));
        for j in 0..N {
            e.intern(key(j));
        }
        for k in 0..FRIENDS {
            let j = (i + 7 * k) % N;
            let score = 1.0 + ((i * 31 + j * 17) % 5) as f32 * 0.25;
            e.rate(j as u32, score, 0);
        }
        nodes.push(e);
    }
    nodes
}

/// 推进一轮传播，返回本轮的 now。
///
/// 时序与仓库里其它真 runtime bin（grid.rs / l2.rs）一致：**先全体导出观点**，
/// 再各自抽样并回填 —— 同一轮里大家看到的是同一批字节，这才是网络语义。
fn advance_round(nodes: &mut [Engine], round: u64) -> u64 {
    let now = round * ROUND_HOURS;
    // 1. 全体快照式发布：这一轮所有观点都基于「轮首」状态。
    let views: Vec<tm_view::CompressedView> = nodes.iter().map(|e| e.export_view(now)).collect();
    // 2. 各自抽样，并把抽到的观点回填（线性下标，200 个节点全在线）。
    for i in 0..nodes.len() {
        let reqs = nodes[i].tick(now, SAMPLES);
        for req in reqs {
            let src = req.idx as usize;
            nodes[i].on_view(req.idx, views[src].clone(), now, now);
        }
    }
    now
}

/// 快照文件里的魔数（应用层自定的容器格式，与引擎快照内部的魔数不是一回事）。
const BLOB_MAGIC: &[u8; 8] = b"P4DEMO\0\0";

/// 把「每台引擎一份引擎快照」打包成一个文件格式：魔数 + 节点数 + now + 逐节点长度前缀。
fn pack(snaps: &[EngineSnapshot], now: u64) -> Vec<u8> {
    let mut blob = Vec::new();
    blob.extend_from_slice(BLOB_MAGIC);
    blob.extend_from_slice(&(snaps.len() as u32).to_le_bytes());
    blob.extend_from_slice(&now.to_le_bytes());
    for snap in snaps {
        let bytes = snap.encode();
        blob.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        blob.extend_from_slice(&bytes);
    }
    blob
}

/// 拆包并解码。任何畸形都返回 None（与引擎侧同一套「绝不 panic」的口径）。
fn unpack(blob: &[u8]) -> Option<(u64, Vec<EngineSnapshot>)> {
    if blob.len() < 8 + 4 + 8 || &blob[..8] != BLOB_MAGIC {
        return None;
    }
    let count = u32::from_le_bytes([blob[8], blob[9], blob[10], blob[11]]) as usize;
    let now = u64::from_le_bytes([
        blob[12], blob[13], blob[14], blob[15], blob[16], blob[17], blob[18], blob[19],
    ]);
    let mut pos = 20usize;
    let mut snaps = Vec::with_capacity(count);
    for _ in 0..count {
        let len = u32::from_le_bytes([
            *blob.get(pos)?,
            *blob.get(pos + 1)?,
            *blob.get(pos + 2)?,
            *blob.get(pos + 3)?,
        ]) as usize;
        pos += 4;
        let end = pos.checked_add(len)?;
        let snap = EngineSnapshot::decode(blob.get(pos..end)?)?;
        snaps.push(snap);
        pos = end;
    }
    if pos != blob.len() {
        return None;
    }
    Some((now, snaps))
}

/// 快照落盘位置：`<仓库根>/.tmp/p_persist_network.bin`（.tmp 已在 .gitignore 里）。
fn snapshot_path() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/exp 的祖先目录必须存在");
    root.join(".tmp").join("p_persist_network.bin")
}

/// 位差：两个 f32 位模式异或后 1 的个数（0 等价于逐位相同）。
fn bit_gap(a: f32, b: f32) -> u32 {
    (a.to_bits() ^ b.to_bits()).count_ones()
}

/// 对比两台引擎在若干时刻、全部 200 个目标上的 query，返回 (最大位差, 最大绝对差, 对比点数)。
fn query_gap(orig: &Engine, restored: &Engine, times: &[u64]) -> (u32, f32, usize) {
    let mut max_bits = 0u32;
    let mut max_abs = 0.0f32;
    let mut points = 0usize;
    for &t in times {
        for target in 0..N as u32 {
            let a = orig.query(target, t);
            let b = restored.query(target, t);
            max_bits = max_bits.max(bit_gap(a, b));
            max_abs = max_abs.max((a - b).abs());
            points += 1;
        }
    }
    (max_bits, max_abs, points)
}

/// 单独给一台引擎拍快照并编码（负对照用）。
fn snap_bytes(e: &Engine, now: u64) -> Vec<u8> {
    e.snapshot(now).encode()
}

/// 对比两台引擎的 shadow 分布（[Engine::trust_distribution]），返回是否逐位相同。
fn shadow_identical(orig: &Engine, restored: &Engine, now: u64) -> bool {
    let a = orig.trust_distribution(now);
    let b = restored.trust_distribution(now);
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|((ai, av), (bi, bv))| ai == bi && bit_gap(*av, *bv) == 0)
}

fn main() {
    println!("P4 状态序列化 · 端到端演示（存盘 → 读回 → 恢复 → 逐位一致）");
    println!(
        "参数：{} 账户 × {} 轮传播 × 每轮抽 {} 个来源；一轮 {}h；视图 {:?}",
        N,
        ROUNDS,
        SAMPLES,
        ROUND_HOURS,
        cfg().view_params
    );

    // ---------- 1. 建网络并跑满 4 轮 ----------
    let mut nodes = build_network();
    let mut now = 0;
    for round in 0..ROUNDS {
        now = advance_round(&mut nodes, round);
    }
    // 4 轮之后的状态必须**非平凡**，否则「恢复后一致」是个空断言。
    let total_views: usize = nodes.iter().map(|e| e.stats().2).sum();
    let total_ratings: usize = nodes.iter().map(|e| e.stats().3).sum();
    let pending: usize = nodes.iter().map(|e| e.round_remaining()).sum();
    println!(
        "\n4 轮传播完成：观点缓存 {} 份、正分条目 {} 条、本轮剩余待抽 {} 个",
        total_views, total_ratings, pending
    );

    // ---------- 2. 拍快照 + 编码 + 落盘 ----------
    let snaps: Vec<EngineSnapshot> = nodes.iter().map(|e| e.snapshot(now)).collect();
    let blob = pack(&snaps, now);
    let path = snapshot_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("创建 .tmp 目录失败");
    }
    fs::write(&path, &blob).expect("写快照文件失败");
    let on_disk = fs::metadata(&path).expect("读文件元数据失败").len();
    println!("\n快照字节数：{} 字节（{} KiB），已写入 {}", blob.len(), blob.len() / 1024, path.display());
    println!(
        "落盘校验：文件实际 {} 字节，每节点平均 {:.1} KiB",
        on_disk,
        blob.len() as f64 / N as f64 / 1024.0
    );
    assert_eq!(on_disk as usize, blob.len(), "文件长度与内存中的编码不一致");

    // ---------- 3. 读回来 + decode + restore（原引擎不动，等价于进程重启） ----------
    let read_back = fs::read(&path).expect("读快照文件失败");
    let (snap_now, decoded) = unpack(&read_back).expect("容器格式解包失败");
    assert_eq!(snap_now, now, "文件里的 now 与内存不一致");
    assert_eq!(decoded.len(), N, "恢复出的引擎数不对");
    let mut restored: Vec<Engine> = Vec::with_capacity(N);
    for (i, snap) in decoded.into_iter().enumerate() {
        let e = Engine::restore(cfg(), snap)
            .unwrap_or_else(|| panic!("第 {i} 台引擎恢复失败（参数指纹或观点字节不匹配）"));
        restored.push(e);
    }
    println!("\n读回 {} 字节，{} 台引擎全部 restore 成功（cfg 与导出时一致）", read_back.len(), restored.len());

    // ---------- 4. 逐位对比 query ----------
    let times = [0u64, now / 2, now, now * 10, now * 10_000];
    let mut max_bits = 0u32;
    let mut max_abs = 0.0f32;
    let mut points = 0usize;
    for i in 0..N {
        let (bits, abs, pts) = query_gap(&nodes[i], &restored[i], &times);
        max_bits = max_bits.max(bits);
        max_abs = max_abs.max(abs);
        points += pts;
    }
    println!(
        "\nquery 位模式对比：{} 个节点 × {} 个时刻 × {} 个目标 = {} 个对比点",
        N, times.len(), N, points
    );
    println!("round-trip 后的最大位差 = {}（0 表示逐位完全相同）", max_bits);
    println!("最大绝对差 = {:.9e}", max_abs);

    // ---------- 5. shadow 分布对比 ----------
    let shadows_ok = (0..N).all(|i| shadow_identical(&nodes[i], &restored[i], now));
    let identical = (0..N).filter(|&i| shadow_identical(&nodes[i], &restored[i], now)).count();
    println!(
        "\n恢复前后 shadow 分布（每个观察者对外可见的信任分布）：{}",
        if shadows_ok {
            format!("一致（{identical}/{N} 个观察者逐位相同）")
        } else {
            format!("不一致（只有 {identical}/{N} 个观察者逐位相同）")
        }
    );
    assert!(shadows_ok, "shadow 分布必须逐位相同");

    // ---------- 6. 继续跑 2 轮：两条网络必须同轨迹 ----------
    // 这一段是真正的验收：前面的对比只证明「读出来的状态对」，这一段证明
    // 「恢复后的引擎采取的**后续动作**也一模一样」（抽样器的 pending 顺序与
    // 随机流位置都装回去了）。
    let mut max_bits_after = 0u32;
    let mut rounds_ok = true;
    for round in ROUNDS..ROUNDS + 2 {
        let now_round = advance_round(&mut nodes, round);
        advance_round(&mut restored, round);
        let mut round_bits = 0u32;
        let mut round_shadow = true;
        for i in 0..N {
            let (bits, _abs, _pts) = query_gap(&nodes[i], &restored[i], &[now_round]);
            round_bits = round_bits.max(bits);
            round_shadow &= shadow_identical(&nodes[i], &restored[i], now_round);
            // 统计与轮次进度也必须同步（抽样没分叉的直接证据）。
            if nodes[i].stats() != restored[i].stats()
                || nodes[i].round_remaining() != restored[i].round_remaining()
            {
                round_shadow = false;
            }
        }
        println!(
            "继续第 {} 轮（t = {}h）：最大位差 = {}，shadow 分布{}",
            round + 1,
            now_round,
            round_bits,
            if round_shadow { "一致" } else { "不一致" }
        );
        max_bits_after = max_bits_after.max(round_bits);
        rounds_ok &= round_shadow;
    }
    assert!(rounds_ok, "恢复后的网络必须与原件继续同轨迹");

    // ---------- 7. 负对照：参数 / 字节错位时必须**拒绝**，而不是悄悄跑歪 ----------
    // 没有这两条，前面的「一致」有可能是「恢复根本没生效」的同义反复。
    let mut bad_cfg = cfg();
    bad_cfg.view_params = ViewParams {
        buckets: BUCKETS,
        hashers: HASHERS,
        seed: VIEW_SEED ^ 0xDEAD,
    };
    let rejected_cfg = nodes
        .iter()
        .all(|e| Engine::restore(bad_cfg.clone(), e.snapshot(now)).is_none());

    // 负对照二：字节被破坏。注意**很多破坏是合法的**——账户表里改掉一个公钥、
    // 观点桶值里改掉一位尾数，解出来仍是一份结构自洽、浮点有限的快照，
    // 引擎当然会接受它（它只是「另一种状态」）。所以这里选两种**必须**被拒的破坏：
    // 截断（长度对不上）与头部指纹被改（参数归属变了）。
    let clean = snap_bytes(&nodes[0], now);
    let rejected_truncated = EngineSnapshot::decode(&clean[..clean.len() - 1]).is_none();
    let mut bad_header = clean.clone();
    bad_header[16] ^= 0x01; // 落在视图参数指纹里（12..20 字节）
    let rejected_header = EngineSnapshot::decode(&bad_header)
        .map(|s| Engine::restore(cfg(), s).is_none())
        .unwrap_or(true);
    let rejected_bytes = rejected_truncated && rejected_header;
    println!(
        "负对照：seed 不同的 cfg 被拒 = {}；截断快照被拒 = {}；指纹被改的快照被拒 = {}",
        rejected_cfg, rejected_truncated, rejected_header
    );
    assert!(rejected_cfg, "参数不同的 cfg 必须拒绝恢复");
    assert!(rejected_bytes, "被破坏的快照必须拒绝");

    // ---------- 8. 结论 ----------
    println!("\n================ 结论 ================");
    println!("快照字节数              = {} 字节", blob.len());
    println!("round-trip 后的最大位差 = {}", max_bits);
    println!("继续 2 轮后的最大位差   = {}", max_bits_after);
    println!("恢复前后 shadow 分布    = {}", if shadows_ok { "一致" } else { "不一致" });
    println!("query 对比点            = {} 个", points);
    println!("最大绝对差              = {:.9e}", max_abs);
    let ok = max_bits == 0 && max_bits_after == 0 && shadows_ok && rejected_cfg && rejected_bytes;
    println!("整体判定                = {}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }

    // 让「抽样流种子互不相同」这条前提有个可观测的落点（否则编译器会嫌 Rng 未使用）。
    let mut rng = Rng::new(engine_seed(0));
    println!(
        "节点 0 的随机流起点 = {:#018x}（各节点种子互不相同，见 engine_seed）",
        rng.state()
    );
    let _ = rng.next_u64();
}
