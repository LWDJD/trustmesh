//! 抽样退避与兜底探测（设计规格 4.9.4）。
//!
//! 这一节**独立于**两级衰减：退避只改变「本轮谁先被问」的抽样顺序，
//! 不改评分、不改 last_check、不改观点缓存，也不参与任何权重。
//!
//! 三条不变量：
//! 1. 失败计入连败、成功清零；改分（rate）**不算**「刚取到观点」，连败保留。
//! 2. 连败 n 次的来源被抽中的概率降为 2^-n，但一轮抽完它仍然会被问到一次。
//! 3. 兜底探测（probe）不走退避：本轮剩下的条目全部返回，按编号升序。

use tm_agg::AggMode;
use tm_engine::{Engine, EngineConfig};
use tm_store::AccountKey;
use tm_view::{CompressedView, ViewParams};

fn key(n: u8) -> AccountKey {
    let mut k = [0u8; 32];
    k[0] = n;
    k[1] = 0xC3;
    k
}

fn base() -> EngineConfig {
    EngineConfig { beta: 0.3, half_life: 72, agg_mode: AggMode::Mean, ..Default::default() }
}

fn engine(cfg: EngineConfig) -> Engine {
    Engine::with_seed(cfg, 42)
}

fn full(target: AccountKey) -> CompressedView {
    CompressedView::from_pairs(ViewParams::default(), &[(target, 1.0)])
}

/// 失败计入连败、成功清零；改分不算「刚取到观点」，连败保留。
#[test]
fn failures_count_for_backoff_and_successes_clear_them() {
    let mut e = engine(base());
    let s = e.intern(key(1));
    e.rate(s, 1.0, 0);
    e.on_view(s, full(key(9)), 0, 0);
    assert_eq!(e.fail_streak(s), 0);
    assert_eq!(e.view_age(s, 0), Some(0));

    // 失败：连败 +1，但 last_check 不动（观点照旧变旧）
    e.on_view_failed(s, 0);
    e.on_view_failed(s, 0);
    assert_eq!(e.fail_streak(s), 2);
    assert_eq!(e.view_age(s, 100), Some(100), "失败不得刷新 last_check");

    // 改分不算「刚取到观点」：连败保留、观点年龄不变
    e.rate(s, 2.0, 200);
    assert_eq!(e.fail_streak(s), 2, "只有成功刷新才清零退避");
    assert_eq!(e.view_age(s, 200), Some(200));

    // 成功：清零 + 年龄归零
    e.on_view(s, full(key(9)), 0, 200);
    assert_eq!(e.fail_streak(s), 0);
    assert_eq!(e.view_age(s, 200), Some(0));

    // 没有评分就没有退避记录
    assert_eq!(e.fail_streak(999), 0);
}

/// 连败的来源被抽中的概率急剧下降（退避 = 权重 1/2^n）。
///
/// 注意「一轮不放回」的语义：退避改变的是**本轮谁先被问**，
/// 连败者会被排到最后 —— 一轮走完它仍然会被问到一次（这是有意的，
/// 否则离线节点永远回不到可抽取集合）。所以这里每轮都从「全员待抽」开始，
/// 只看每一轮的**第一抽**。
#[test]
fn tick_backs_off_sources_that_keep_failing() {
    let mut e = engine(base());
    for i in 1..=3u8 {
        let s = e.intern(key(i));
        e.rate(s, 1.0, 0);
        e.on_view(s, full(key(9)), 0, 0);
    }
    let bad = e.intern(key(1)); // 编号 0
    for _ in 0..20 {
        e.on_view_failed(bad, 0);
    }
    assert_eq!(e.fail_streak(bad), 20);

    // 每轮的第一抽：连败 20 次的权重是 2^-20，六百分之一都不到
    let mut counts = [0usize; 3];
    for _ in 0..600 {
        e.restart_round();
        for req in e.tick(0, 1) {
            counts[req.idx as usize] += 1;
        }
    }
    assert!(counts[0] <= 2, "连败 20 次的来源权重是 2^-20，第一抽几乎抽不到，实际 {} 次", counts[0]);
    assert!(counts[1] > 250 && counts[2] > 250, "另外两个应各占约一半，实际 {:?}", counts);

    // 一轮走完：连败者必然被排到最后（这是「不放回」的必然结果，也是兜底恢复的前提）
    e.restart_round();
    let mut round = Vec::new();
    for _ in 0..3 {
        round.extend(e.tick(0, 1).into_iter().map(|r| r.idx));
    }
    assert_eq!(round.len(), 3);
    assert_ne!(round[0], 0, "连败最重的来源不该第一个被问：{round:?}");
    assert_eq!(round[2], 0, "它应当最后被问：{round:?}");
}

/// 兜底探测不看退避：本轮剩下的全部返回，按编号升序，且不重复已抽过的。
#[test]
fn probe_covers_the_whole_round_without_backoff() {
    let mut e = engine(base());
    for i in 1..=4u8 {
        let s = e.intern(key(i));
        e.rate(s, 1.0, 0);
        e.on_view(s, full(key(9)), 0, 0);
    }
    let bad = e.intern(key(3));
    for _ in 0..30 {
        e.on_view_failed(bad, 0);
    }

    let first = e.tick(0, 1);
    assert_eq!(first.len(), 1);
    assert_eq!(e.round_remaining(), 3);

    let rest = e.probe(0);
    assert_eq!(rest.len(), 3, "兜底探测必须覆盖本轮剩下的全部条目");
    let ids: Vec<u32> = rest.iter().map(|r| r.idx).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "探测结果按编号升序");
    assert!(ids.contains(&bad), "连败最严重的来源也必须被探测到");

    let mut all = ids;
    all.push(first[0].idx);
    all.sort_unstable();
    assert_eq!(all, vec![0, 1, 2, 3], "一轮里两次调用恰好覆盖全部条目");
    assert_eq!(e.round_remaining(), 0);
}
