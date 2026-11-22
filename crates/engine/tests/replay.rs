//! 缺陷 D4（重放旧观点 / 无 published_at 单调性）的**现状刻画测试**（安全加固 P2）。
//!
//! 本文件**不改任何行为**：它把「当前实现确实允许这些事」钉成可执行证据，
//! 供 `审计/P2-security.md` §5 的「修 / 不修」论证引用。
//!
//! # 钉住的三件事
//!
//! 1. **重放会无限刷新 last_check**：on_view 只看 now，不看 published_at，
//!    也不比较内容版本。同一份旧字节可以反复把衰减基准推到「刚刚」，
//!    于是这个来源在我眼里的权重被永久恢复到 1.0（衰减线失效）。
//! 2. **攻击者的最佳策略变成「定期重放同一份字节」**：它不需要写任何新观点，
//!    只要每隔一段时间重放一次旧字节，群体项就能被推到 1.0。
//! 3. **published_at 可以回滚内容**：先收发布时间 500 的新观点，再收发布时间 100
//!    的旧观点，引擎照收——后者覆盖前者，读数倒退回旧值。
//!
//! # 为什么写成「绿」的测试
//!
//! 这三条现在是**已知未修**的结构性缺陷（审计 A8-attack D4 判 A 级，并明确
//! 「不能只靠库修」）。把它们写成失败测试会让 cargo test 长期红，失去回归价值；
//! 写成 ignored 测试则日常根本不会跑。这里选择「断言现状」：
//!
//! * 谁要是将来修了 D4（例如加了 published_at 单调性检查 / 内容版本号），
//!   这些测试会**立刻变红**，提醒他同步更新审计文档与本节注释；
//! * 谁要是无意中把重放路径弄坏了（比如 on_view 开始因为别的原因拒绝重复字节），
//!   也会立刻变红——那是另一个方向的回归。
//!
//! 换句话说：它们是「缺陷的温度计」，不是「契约」。修 D4 时应当把它们改成
//! 断言「拒绝」，而不是删掉。

use tm_engine::view::{AccountKey, CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};

fn key(i: u32) -> AccountKey {
    let mut k = [0u8; 32];
    k[0..4].copy_from_slice(&i.to_le_bytes());
    k[8] = 0xA5;
    k
}

fn params() -> ViewParams {
    ViewParams {
        buckets: 512,
        hashers: 5,
        seed: 7,
    }
}

fn eng() -> Engine {
    Engine::with_seed(
        EngineConfig {
            beta: 0.3,
            half_life: 72,
            view_params: params(),
            ..Default::default()
        },
        42,
    )
}

/// 一份只对 target 表态的观点。score = 0 的视图仍然是合法字节（只是对 target 没表态），
/// 重放时逐字节相同。
fn view_of(target: AccountKey, score: f32) -> CompressedView {
    CompressedView::from_pairs(params(), &[(target, score)])
}

/// 群体项：从最终声望反解 —— 声望 = β·个人项 + (1−β)·群体项。
///
/// 本文件所有用例的**观测目标都没有被打过分**，所以个人项恒为 0，
/// 群体项 = query / (1 − β) = query / 0.7。这样反解一步到位，
/// 测的仍然是 `Engine::query` 的同一条路径，不是另算一遍。
fn group_term(e: &Engine, target: u32, now: u64) -> f32 {
    e.query(target, now) / 0.7
}

// ===========================================================================
// 1. 重放旧观点 ⟹ last_check 被无限刷新 ⟹ 权重被永久恢复到 1.0
// ===========================================================================

/// 同一份**旧字节**反复 on_view，last_check 每次都刷新，攻击者的权重永远回到 1.0。
///
/// 场景（两人各 50% 份额，观测目标没被打分 ⇒ 个人项 = 0）：
///
/// * H（诚实来源）在 t=288 刷新了观点「target = 0」，权重 1；
/// * A（攻击者）的观点「target = 1」停在 t=0，到 t=288 时权重只剩 0.5^4 = 0.0625。
///
/// 于是 t=288 时的群体项 = 0.0625 / 1.0625 ≈ **5.88%**（攻击者几乎被衰减压没）。
/// 攻击者把 t=0 那份**完全相同**的字节重放一次（published_at 仍是 0），
/// 权重立刻回到 1.0，群体项变成 1 / (1+1) = **50%** —— **放大 8.5 倍**。
///
/// 若修了 D4：这份重放必须被拒绝（发布时间没有前进），群体项应当停在 5.88%。
#[test]
fn d4_characterization_replay_refreshes_last_check_and_restores_full_weight() {
    let mut e = eng();
    let h = e.intern(key(1)); // 诚实来源
    let a = e.intern(key(2)); // 攻击者
    let target = e.intern(key(9));
    e.rate(h, 1.0, 0);
    e.rate(a, 1.0, 0);

    // t = 0：两份观点都在 0 时刻被取到。
    e.on_view(h, view_of(key(9), 0.0), 0, 0);
    e.on_view(a, view_of(key(9), 1.0), 0, 0);

    // t = 288（4 个半衰期）：H 刷新自己的观点，A 不刷新。
    e.on_view(h, view_of(key(9), 0.0), 0, 288);
    assert_eq!(
        e.view_age(a, 288),
        Some(288),
        "A 的观点确实已经 288 个时间单位没更新"
    );

    let before = group_term(&e, target, 288);
    assert!(
        (before - 0.0625 / 1.0625).abs() < 1e-4,
        "衰减必须先把攻击者的权重压到 1/17，实际群体项 {before}"
    );

    // —— 重放：同一份旧字节，published_at 仍然是 0，只有 now 变成 288 ——
    e.on_view(a, view_of(key(9), 1.0), 0, 288);
    assert_eq!(
        e.view_age(a, 288),
        Some(0),
        "重放把「观点年龄」直接钉回 0 —— 衰减基准是 last_check，而它被刷新了"
    );
    let after = group_term(&e, target, 288);
    assert!(
        (after - 0.5).abs() < 1e-4,
        "重放之后 A 的权重回到 1.0，群体项必须是 0.5（这就是缺陷本身），实际 {after}"
    );
    let ratio = after / before;
    assert!(ratio > 8.0, "放大倍率应当约 8.5 倍，实测 {ratio:.2}");
}

/// 攻击者的最优策略：写一份观点，然后**每隔一段时间重放同一份字节**。
///
/// 第 r 轮重放后（now = 288 + r·72）：A 权重恒 1（刚「刷新」），
/// 而 H 停在 288 不动，衰减成 0.5^r。群体项 = 1 / (1 + 0.5^r) → 迅速逼近 1.0。
///
/// 这正是审计 A8-attack 记录的「0.25 → 0.997」现象在单来源上的最小复现：
/// **攻击不需要任何新内容，只需要重复投递旧内容。**
#[test]
fn d4_characterization_periodic_replay_pushes_group_term_to_one() {
    let mut e = eng();
    let h = e.intern(key(1));
    let a = e.intern(key(2));
    let target = e.intern(key(9));
    e.rate(h, 1.0, 0);
    e.rate(a, 1.0, 0);
    e.on_view(h, view_of(key(9), 0.0), 0, 0);
    e.on_view(a, view_of(key(9), 1.0), 0, 0);
    e.on_view(h, view_of(key(9), 0.0), 0, 288);

    let mut last = 0.0f32;
    for round in 0..40u64 {
        let now = 288 + round * 72;
        e.on_view(a, view_of(key(9), 1.0), 0, now); // 重放，内容与 published_at 都不变
        assert_eq!(
            e.view_age(a, now),
            Some(0),
            "第 {round} 轮重放：年龄被钉回 0"
        );
        let g = group_term(&e, target, now);
        // 期望值：A 权重 = 1（刚刷新），H 权重 = 0.5^round。
        let expected = (1.0f32) / (1.0 + 0.5f32.powi(round as i32));
        assert!(
            (g - expected).abs() < 1e-4,
            "第 {round} 轮：群体项应为 {expected}，实际 {g}"
        );
        assert!(g >= last, "每轮重放只会让攻击者份额单调上升：{last} -> {g}");
        last = g;
    }
    assert!(
        last > 0.999,
        "40 轮重放之后群体项必须逼近 1.0（实测 {last}）"
    );
}

/// 重放不需要攻击者付出任何成本：字节、签名、发布时间全都不用变。
///
/// 重放的输入字节完全相同（encode() 逐字节一致），唯一的差别是 now。
#[test]
fn d4_characterization_replay_needs_identical_bytes() {
    let mut e = eng();
    let a = e.intern(key(2));
    e.rate(a, 1.0, 0);
    let bytes = view_of(key(9), 1.0).encode();
    let replay = CompressedView::decode(&bytes, params()).expect("自己编码的字节必须可解码");

    for round in 0..3u64 {
        let now = round * 1000;
        e.on_view(a, replay.clone(), 0, now);
        assert_eq!(e.view_age(a, now), Some(0));
    }
    assert_eq!(replay.encode(), bytes, "重放的字节必须逐字节相同");
}

// ===========================================================================
// 2. published_at 不参与任何判断 ⟹ 旧观点可以回滚新内容
// ===========================================================================

/// on_view 收下 published_at 却从不比较它：更旧的发布时间照样覆盖更新的内容。
///
/// 场景：t=100 收到「target 1.0」（声明发布于 500）；t=200 收到「target 0.0」
/// （声明发布于 **100**，比上一份还旧）。引擎照收第二份，读数从 0.7 回落到 0 ——
/// 这是一次**内容回滚**，正常情况下（单调性检查）它应当被拒绝。
///
/// 注意：published_at 本身是对方随便填的，所以「比较 published_at」只能挡住
/// 网络层的重复投递/乱序，挡不住伪造。真正的裁决权在应用层（签名时间戳 / 版本号）。
#[test]
fn d4_characterization_older_published_at_can_roll_back_newer_content() {
    let mut e = eng();
    let a = e.intern(key(2));
    let target = e.intern(key(9));
    e.rate(a, 1.0, 0);

    e.on_view(a, view_of(key(9), 1.0), 500, 100);
    // 个人项 = 0（目标没被打分），群体项 = 1 ⇒ query = 0.7
    let newer = e.query(target, 100);
    assert!(
        (newer - 0.7).abs() < 1e-4,
        "先收下的新观点必须生效（群体项 = 1 ⇒ 声望 = 0.7），实际 {newer}"
    );

    // 再收一份**声明发布时间更早**的旧内容，引擎不会拒绝。
    e.on_view(a, view_of(key(9), 0.0), 100, 200);
    let rolled_back = e.query(target, 200);
    assert!(
        rolled_back.abs() < 1e-4,
        "内容被成功回滚：群体项从 1.0 掉到 0，声望跟着掉到 0，实际 {rolled_back}"
    );

    // 观察到的 last_check 也前移了 —— 「我什么时候拿到」和「内容有多新」被彻底解耦。
    assert_eq!(e.view_age(a, 200), Some(0));
}

/// 结论性的量化：同一份旧观点可以在任意远的未来被「重新变新鲜」。
///
/// 不重放的话，t = 288 + 100×72 时刻的权重是 0.5^104 ≈ 4.9e-32 —— 早已等于没有。
#[test]
fn d4_characterization_decay_would_have_killed_the_view_without_replay() {
    let dt = 288u64 + 100 * 72;
    let honest = tm_engine::agg::decay(dt, 72);
    assert!(
        honest < 1e-30,
        "不重放的话，104 个半衰期之后权重是 0.5^104 ≈ 4.9e-32，实际 {honest:e}"
    );
    assert!(
        honest > 0.0,
        "衰减只会无限趋近于 0，永不精确归零（DECAY_FLOOR 的约定）"
    );
}
