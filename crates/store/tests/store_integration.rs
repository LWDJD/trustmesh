//! 集成测试：从 crate 外部、只通过公开 API 跑端到端场景。
//!
//! lib.rs 里的单元测试逐条验证单个结构的语义；本文件补的是
//! 「三个结构串起来用」时才会暴露的问题：
//!
//! 账户表定址 -> 正分清单记分 -> 抽样器取人 -> 成功/失败分别处理
//! -> 评分被撤销后用 sync 把死账号踢出本轮。
//!
//! 所有随机性都由固定种子的 Rng 驱动，因此本文件里的断言是确定性的，
//! 不会出现「偶尔红一次」的抖动。

use std::collections::BTreeSet;

use tm_store::{AccountIdx, AccountKey, AccountTable, PositiveList, Rng, RoundSampler};
#[allow(unused_imports)]
use tm_store::MAX_SCORE;

/// 构造互不相同的测试公钥（只动前 4 字节，其余留 0）。
fn key(n: u32) -> AccountKey {
    let mut k = [0u8; 32];
    k[..4].copy_from_slice(&n.to_be_bytes());
    k
}

/// 把切片收成集合，方便断言「集合相等」而不关心顺序。
fn set_of(v: &[AccountIdx]) -> BTreeSet<AccountIdx> {
    v.iter().copied().collect()
}

/// 构造 n 个连续编号 [0, n)。
fn range(n: u32) -> Vec<AccountIdx> {
    (0..n).collect()
}

// ===========================================================================
// 1. 账户表：定址与幂等
// ===========================================================================

#[test]
fn account_table_roundtrips_every_key() {
    let mut table = AccountTable::new();
    let mut seen = Vec::new();
    for n in 0..256u32 {
        let idx = table.intern(key(n));
        assert_eq!(idx, n, "编号必须从 0 连续递增");
        seen.push(idx);
    }
    assert_eq!(table.len(), 256);
    for n in 0..256u32 {
        let idx = seen[n as usize];
        assert_eq!(table.lookup(&key(n)), Some(idx));
        assert_eq!(table.key_of(idx), Some(key(n)));
    }
}

#[test]
fn intern_is_idempotent_under_repetition() {
    let mut table = AccountTable::new();
    let a = table.intern(key(1));
    let b = table.intern(key(2));
    // 大量重复 intern 不得推进编号游标，也不得改变已有映射。
    for _ in 0..1000 {
        assert_eq!(table.intern(key(1)), a);
        assert_eq!(table.intern(key(2)), b);
    }
    assert_eq!(table.len(), 2);
    assert_eq!(table.intern(key(3)), 2, "下一个新 key 仍拿到连续的 2");
}

#[test]
fn account_table_has_no_index_recycling() {
    // 编号永不回收：本 crate 刻意不提供 remove/clear，
    // 因此「已分配编号」永远可用 key_of 取回。这里用越界查询确认边界。
    let mut table = AccountTable::new();
    let idx = table.intern(key(9));
    assert_eq!(idx, 0);
    assert_eq!(table.key_of(1), None, "len 及以上的编号尚未分配");
    assert_eq!(table.key_of(AccountIdx::MAX), None);
    assert_eq!(table.lookup(&key(10)), None);
}

// ===========================================================================
// 2. 正分清单：只收正分、只刷新时间
// ===========================================================================

#[test]
fn only_positive_scores_are_stored() {
    let mut list = PositiveList::new();
    list.set(1, 0.75, 10);
    assert_eq!(list.get(1), Some(0.75));

    // 0 / -0.0 / 负数 / NaN 都表示「没表态」，等价于删除。
    let bad_scores = [0.0f32, -0.0, -0.25, -1.0, f32::NAN];
    for (i, bad) in bad_scores.iter().enumerate() {
        let idx = 100 + i as AccountIdx;
        list.set(idx, 0.5, 10);
        assert!(list.has(idx), "先要有条目才能验证删除");
        list.set(idx, *bad, 20);
        assert!(!list.has(idx), "score <= 0 必须移除条目（bad = {bad}）");
        assert_eq!(list.get(idx), None);
        assert_eq!(list.last_check(idx), None, "删除后不留任何痕迹");
    }
    assert_eq!(list.len(), 1, "只有 0.75 那一条应该存活");
}

#[test]
fn scores_stay_within_positive_bounded() {
    // 应用层给的是 0~n 的原始数值（互动次数、点赞数……），库负责换算成占比。
    // 存储层只守两条：必须是有限正数，且不超过合理性上界。
    let mut list = PositiveList::new();
    list.set(1, 0.000_001, 0);
    list.set(2, 1.0, 0);
    list.set(3, 7.5, 0);
    list.set(4, f32::INFINITY, 0);
    for idx in 1..=4 {
        let s = list.get(idx).unwrap();
        assert!(s > 0.0 && s <= MAX_SCORE, "评分越界：{idx} -> {s}");
        assert!(s.is_finite(), "存储值必须有限：{idx} -> {s}");
    }
    assert!(list.get(1).unwrap() > 0.0);
    assert_eq!(list.get(2), Some(1.0));
    assert_eq!(list.get(3), Some(7.5), "0~n 的原始数值不得被截断");
    assert_eq!(list.get(4), Some(MAX_SCORE), "无穷大收敛到上界");
}

#[test]
fn set_refreshes_time_and_touch_only_moves_time() {
    let mut list = PositiveList::new();
    list.set(3, 0.6, 100);
    assert_eq!(list.last_check(3), Some(100));

    // 分数不变也要刷新时间（重新表态就是重新确认关系）。
    list.set(3, 0.6, 250);
    assert_eq!(list.last_check(3), Some(250));
    assert_eq!(list.get(3), Some(0.6));

    // touch 只动时间，不动分数，是「成功取到观点」的唯一标记。
    list.touch(3, 400);
    assert_eq!(list.last_check(3), Some(400));
    assert_eq!(list.get(3), Some(0.6));
    assert_eq!(list.len(), 1);

    // 对不存在的条目 touch 是空操作，不能凭空造一条记录。
    list.touch(99, 400);
    assert!(!list.has(99));
    assert_eq!(list.last_check(99), None);
}

#[test]
fn touch_after_removal_does_not_resurrect_entry() {
    let mut list = PositiveList::new();
    list.set(7, 0.9, 10);
    list.set(7, 0.0, 20);
    list.touch(7, 30);
    assert!(!list.has(7));
    assert_eq!(list.get(7), None);
    assert_eq!(list.len(), 0);
}

#[test]
fn time_is_opaque_and_not_forced_monotonic() {
    // 时间由上层给（秒或毫秒皆可），存储层只如实记录，不假设单调递增。
    let mut list = PositiveList::new();
    list.set(1, 0.5, 1000);
    list.set(1, 0.5, 10);
    assert_eq!(list.last_check(1), Some(10), "时间回拨必须被如实保存");
    assert_eq!(list.get(1), Some(0.5));
}

#[test]
fn iter_matches_get_and_is_sorted_by_index() {
    let mut list = PositiveList::new();
    let entries = [(9u32, 0.9f32, 90u64), (2, 0.2, 20), (5, 0.5, 50)];
    for &(idx, score, t) in &entries {
        list.set(idx, score, t);
    }
    let got: Vec<_> = list.iter().collect();
    assert_eq!(got.len(), list.len());
    assert_eq!(got, vec![(2, 0.2, 20), (5, 0.5, 50), (9, 0.9, 90)]);
    // 每一行都要与单点查询一致
    for (idx, score, t) in got {
        assert_eq!(list.get(idx), Some(score));
        assert_eq!(list.last_check(idx), Some(t));
    }
}

// ===========================================================================
// 3. 抽样器：均匀、不放回、抽完自动重置
// ===========================================================================

#[test]
fn draw_returns_only_rated_accounts_and_never_duplicates() {
    let mut table = AccountTable::new();
    let mut list = PositiveList::new();
    let mut rated = BTreeSet::new();
    for n in 0..20u32 {
        let idx = table.intern(key(n));
        if n % 5 != 0 {
            list.set(idx, 0.5, 1000); // 16 个评过分
            rated.insert(idx);
        }
    }

    let mut sampler = RoundSampler::new();
    sampler.reset(&list.indices());
    assert_eq!(sampler.remaining(), 16);

    let mut rng = Rng::new(0xC0FF_EE00_1234_5678);
    let picked = sampler.draw(5, &mut rng);
    assert_eq!(picked.len(), 5);
    assert_eq!(set_of(&picked).len(), 5, "一轮内不得重复");
    for idx in &picked {
        assert!(rated.contains(idx), "抽到了没评过分的人：{idx}");
        assert!(list.has(*idx));
    }
    assert_eq!(sampler.remaining(), 11);
}

#[test]
fn full_round_visits_every_member_exactly_once() {
    // 37 是质数：不整除任何常见批量，能暴露「下标越界 / 结尾漏抽」类错误。
    let members = range(37);
    let mut sampler = RoundSampler::new();
    sampler.reset(&members);

    let mut rng = Rng::new(0x1234_ABCD_5678_EF01);
    let mut seen = BTreeSet::new();
    while sampler.remaining() > 0 {
        let out = sampler.draw(1, &mut rng);
        assert_eq!(out.len(), 1, "还有剩余时 draw(1) 必须给出 1 个");
        assert!(seen.insert(out[0]), "同一轮内重复抽到 {}", out[0]);
    }
    assert_eq!(seen, set_of(&members), "一轮必须恰好覆盖全部成员");
    assert_eq!(seen.len(), 37);
}

#[test]
fn draw_beyond_remaining_returns_only_remainder_then_resets() {
    let members = range(5);
    let mut sampler = RoundSampler::new();
    sampler.reset(&members);
    let mut rng = Rng::new(42);

    let first = sampler.draw(3, &mut rng);
    assert_eq!(first.len(), 3);
    assert_eq!(sampler.remaining(), 2);

    // k 远大于剩余：只返回剩下的，不在本轮内绕回去重复。
    let rest = sampler.draw(100, &mut rng);
    assert_eq!(rest.len(), 2);
    assert_eq!(sampler.remaining(), 0);
    let mut all = first.clone();
    all.extend(rest);
    assert_eq!(all.len(), 5);
    assert_eq!(set_of(&all), set_of(&members), "两批合起来是全集且不重复");

    // 已抽空：下一次 draw 自动重置为全量。
    let next_round = sampler.draw(2, &mut rng);
    assert_eq!(next_round.len(), 2);
    assert_eq!(sampler.remaining(), 3);
    for idx in &next_round {
        assert!(members.contains(idx), "自动重置只能从全量清单里抽");
    }
}

#[test]
fn draw_zero_does_not_consume_or_reset() {
    let mut sampler = RoundSampler::new();
    sampler.reset(&range(4));
    let mut rng = Rng::new(7);

    assert!(sampler.draw(0, &mut rng).is_empty());
    assert_eq!(sampler.remaining(), 4, "k=0 不消耗本轮名额");

    assert_eq!(sampler.draw(4, &mut rng).len(), 4);
    assert_eq!(sampler.remaining(), 0);
    // 本轮已空时 draw(0) 也不应触发重置（否则会白白打乱轮次边界）。
    assert!(sampler.draw(0, &mut rng).is_empty());
    assert_eq!(sampler.remaining(), 0);
}

#[test]
fn empty_inputs_are_safe() {
    let mut sampler = RoundSampler::new();
    let mut rng = Rng::new(1);
    assert!(sampler.draw(5, &mut rng).is_empty());
    assert_eq!(sampler.remaining(), 0);

    sampler.reset(&[]);
    assert_eq!(sampler.remaining(), 0);
    assert!(sampler.draw(usize::MAX, &mut rng).is_empty());

    let empty = PositiveList::new();
    sampler.sync(&empty.indices());
    assert!(sampler.draw(1, &mut rng).is_empty(), "空清单抽不出人");
}

// ===========================================================================
// 4. 失败路径：不 touch、不重试、不阻塞
// ===========================================================================

#[test]
fn failed_fetch_leaves_timestamp_untouched() {
    // 一条「取观点」的正常路径：抽到 -> 尝试取 -> 成功才 touch，失败什么都不做。
    let mut list = PositiveList::new();
    list.set(10, 0.9, 1000);
    list.set(11, 0.5, 1000);
    list.set(12, 0.2, 1000);

    let mut sampler = RoundSampler::new();
    sampler.reset(&list.indices());
    let mut rng = Rng::new(0xFEED_FACE);
    let drawn = sampler.draw(2, &mut rng);
    let (ok, failed) = (drawn[0], drawn[1]);

    // 第一个成功取到观点；第二个对端超时（失败是正常路径）。
    list.touch(ok, 1072);
    // 失败分支：不 touch、不重试、不删除、不阻塞 —— 调用方什么都不用做。
    let score_of_failed = list.get(failed);
    assert!(score_of_failed.is_some());

    assert_eq!(list.last_check(ok), Some(1072), "成功才刷新时间戳");
    assert_eq!(list.last_check(failed), Some(1000), "失败不得刷新时间戳");
    assert_eq!(list.len(), 3, "失败不得删除条目");
    assert_eq!(sampler.remaining(), 1, "失败不得把名额退回本轮");

    // 失败者仍然留在清单里，下一轮照常参与抽样。
    let rest = sampler.draw(1, &mut rng);
    assert_eq!(rest.len(), 1);
    assert!(list.has(rest[0]) || rest[0] == ok, "抽到的必须仍在清单里");
    match list.get(rest[0]) {
        Some(s) => assert!(s > 0.0 && s <= 1.0),
        None => assert_eq!(rest[0], ok, "只有已抽走的那位可能不在清单里"),
    }
}

// ===========================================================================
// 5. sync：剔除死账号、保留未抽中的、不提前放新账号进来
// ===========================================================================

#[test]
fn sync_prunes_deleted_accounts_from_pending() {
    let mut table = AccountTable::new();
    let mut list = PositiveList::new();
    for n in 0..12u32 {
        let idx = table.intern(key(n));
        list.set(idx, 0.8, 1000);
    }

    let mut sampler = RoundSampler::new();
    sampler.reset(&list.indices());
    let mut rng = Rng::new(0x0BAD_C0DE);
    let drawn = sampler.draw(3, &mut rng);
    assert_eq!(sampler.remaining(), 9);

    // 在已抽中的里面挑一个撤销评分，再从「尚未抽中」的里面挑一个撤销评分。
    let x = drawn[0];
    let pending_before: BTreeSet<AccountIdx> = range(12)
        .into_iter()
        .filter(|i| !drawn.contains(i))
        .collect();
    let y = *pending_before.iter().next().unwrap();

    list.set(x, 0.0, 2000);
    list.set(y, 0.0, 2000);
    sampler.sync(&list.indices());

    // 待抽集合原本 9 个，其中 y 被删 -> 8 个；已抽中的 x 本来就不在集合里。
    assert_eq!(sampler.remaining(), 8, "死账号必须从待抽集合剔除");

    let rest = sampler.draw(100, &mut rng);
    assert_eq!(rest.len(), 8, "k 大于剩余时只返回剩余的");
    assert!(!rest.contains(&y), "被删除的账户不得再被抽到");
    assert!(!rest.contains(&x));
    assert_eq!(
        rest.iter().copied().collect::<BTreeSet<_>>().len(),
        8,
        "不得重复"
    );

    // 本轮抽完后自动重置：此时全量清单只剩 10 个活着的账户。
    let next = sampler.draw(100, &mut rng);
    assert_eq!(next.len(), 10, "自动重置应使用 sync 后的全量清单");
    assert!(!next.contains(&x) && !next.contains(&y));
    assert_eq!(next.iter().copied().collect::<BTreeSet<_>>().len(), 10);
}

#[test]
fn sync_keeps_undrawn_and_defers_newcomers() {
    let mut sampler = RoundSampler::new();
    sampler.reset(&[0, 1, 2, 3]);
    let mut rng = Rng::new(0x5EED);
    let first = sampler.draw(2, &mut rng);
    assert_eq!(sampler.remaining(), 2);

    // 本轮进行中新增两个账户：sync 只更新全量清单，不往本轮塞人。
    sampler.sync(&[0, 1, 2, 3, 4, 5]);
    assert_eq!(sampler.remaining(), 2, "本轮进行中不补充新成员");

    let rest = sampler.draw(10, &mut rng);
    assert_eq!(rest.len(), 2, "k 大于剩余时只返回剩余的");
    let mut all = first.clone();
    all.extend(rest);
    assert_eq!(set_of(&all), set_of(&[0, 1, 2, 3]), "本轮恰好是原集合");

    // 下一轮才包含新成员。
    let next = sampler.draw(100, &mut rng);
    assert_eq!(next.len(), 6);
    assert_eq!(set_of(&next), set_of(&[0, 1, 2, 3, 4, 5]));
}

#[test]
fn sync_is_idempotent_and_never_resurrects_drawn() {
    let mut sampler = RoundSampler::new();
    sampler.reset(&range(10));
    let mut rng = Rng::new(0xDEAD_BEEF);
    let drawn = sampler.draw(4, &mut rng);
    for _ in 0..5 {
        sampler.sync(&range(10));
    }
    assert_eq!(sampler.remaining(), 6, "sync 不得回退本轮进度");
    let rest = sampler.draw(100, &mut rng);
    assert_eq!(rest.len(), 6);
    for idx in &rest {
        assert!(!drawn.contains(idx), "已抽中的 {idx} 不得被 sync 放回");
    }
}

#[test]
fn sync_on_fresh_sampler_then_draw_starts_a_round() {
    // 常见调用顺序：new -> sync(全量) -> draw
    let mut sampler = RoundSampler::new();
    sampler.sync(&range(3));
    assert_eq!(sampler.remaining(), 0, "sync 只记录全量，不主动开轮");

    let mut rng = Rng::new(11);
    let out = sampler.draw(2, &mut rng);
    assert_eq!(out.len(), 2, "draw 会自动用 sync 记录的全量开新一轮");
    assert_eq!(sampler.remaining(), 1);
}

// ===========================================================================
// 6. 均匀性：抽样只跟「在不在清单里」有关，与评分大小无关
// ===========================================================================

#[test]
fn sampling_ignores_score_weights_with_99_low_scores() {
    // 规格的核心证明：1 个 1.0 加 99 个 0.001，跑 10 万次抽样。
    // 若有任何按评分加权的实现，那 99 个 0.001 的账户几乎抽不到。
    //
    // 注意：不放回抽样 + 抽完自动重置意味着每一轮都是一个置换，
    // 100 个成员、10 万次 k=1 恰好是 1000 整轮，因此每个账户应恰好出现
    // 1000 次（偏差 0）。这比 5% 的频率检验更严格。
    const N: usize = 100;
    const SAMPLES: usize = 100_000;

    let mut list = PositiveList::new();
    list.set(0, 1.0, 1000);
    for idx in 1..N as AccountIdx {
        list.set(idx, 0.001, 1000);
    }
    let mut sampler = RoundSampler::new();
    sampler.reset(&list.indices());

    let mut rng = Rng::new(0x2468_ACE0_1357_9BDF);
    let mut counts = [0usize; N];
    let mut seen_in_round = BTreeSet::new();
    for _ in 0..SAMPLES {
        let out = sampler.draw(1, &mut rng);
        assert_eq!(out.len(), 1);
        let idx = out[0] as usize;
        assert!(idx < N);
        assert!(seen_in_round.insert(idx), "同一轮内重复抽到 {idx}");
        if seen_in_round.len() == N {
            seen_in_round.clear();
        }
        counts[idx] += 1;
    }

    let expected = SAMPLES as f64 / N as f64; // 1000
    let band = expected * 0.05; // 5% 相对偏差
    for (i, &c) in counts.iter().enumerate() {
        let dev = (c as f64 - expected).abs();
        assert!(
            dev < band,
            "账户 {i}（评分 {}）出现 {c} 次，期望 {expected:.1}，偏差 {dev:.1} 超过 5%（上限 {band:.1}）——抽样被评分加权了？",
            if i == 0 { 1.0 } else { 0.001 }
        );
    }
    assert_eq!(counts.iter().sum::<usize>(), SAMPLES);
    assert_eq!(counts[0], 1000, "1.0 分账户没有特权");
    assert_eq!(counts[99], 1000, "0.001 分账户没有被饿死");
}

#[test]
fn first_pick_after_reset_is_uniform_over_100k() {
    // 真正有统计意义的均匀性检验：每次 reset 后只抽 1 个，消除
    // 「不放回 + 自动重置」带来的轮转结构，直接检验洗牌的边缘分布。
    // 10 个成员、10 万样本 => 期望 10000、5% 带 = 500、标准差约 95，
    // 即约 5.3 个标准差，误报概率可忽略。
    const N: usize = 10;
    const SAMPLES: usize = 100_000;
    let members = range(N as u32);

    let mut sampler = RoundSampler::new();
    let mut rng = Rng::new(0x1357_9BDF_2468_ACE0);
    let mut counts = [0usize; N];
    for _ in 0..SAMPLES {
        sampler.reset(&members);
        let out = sampler.draw(1, &mut rng);
        assert_eq!(out.len(), 1);
        counts[out[0] as usize] += 1;
    }

    let expected = SAMPLES as f64 / N as f64;
    let band = expected * 0.05;
    for (i, &c) in counts.iter().enumerate() {
        let dev = (c as f64 - expected).abs();
        assert!(
            dev < band,
            "首抽成员 {i} 出现 {c} 次，期望 {expected:.1}，偏差 {dev:.1} 超过 5%"
        );
    }
    assert_eq!(counts.iter().sum::<usize>(), SAMPLES);
}

#[test]
fn batch_draw_is_uniform_over_100k() {
    // 批量抽取同样均匀：10 个成员，每次抽 3 个，共 10 万个样本。
    const N: usize = 10;
    const CALLS: usize = 33_335; // 33_335 * 3 = 100_005 个样本
    const PER: usize = 3;

    let members = range(N as u32);
    let mut sampler = RoundSampler::new();
    sampler.reset(&members);
    let mut rng = Rng::new(0x0F1E_2D3C_4B5A_6978);

    let mut counts = [0usize; N];
    for _ in 0..CALLS {
        // 一次调用可能正好跨过轮次边界：本轮只剩 1 个时只能拿到 1 个，
        // 本轮已空时 draw 会自动开新一轮并给满 k 个。
        let cap = if sampler.remaining() == 0 {
            members.len()
        } else {
            sampler.remaining()
        };
        let want = cap.min(PER);
        let out = sampler.draw(PER, &mut rng);
        assert_eq!(out.len(), want, "draw(k) 应返回 min(k, 本轮剩余)");
        for idx in out {
            counts[idx as usize] += 1;
        }
    }

    // 10 个成员、每次抽 3 个：一轮要 4 次调用（3+3+3+1），最后那次只给 1 个，
    // 名额有约 17% 的浪费。因此样本总数会小于 CALLS * PER，但仍在 8 万以上。
    let total: usize = counts.iter().sum();
    assert!(total <= CALLS * PER, "抽样总数不可能超过请求总数");
    assert!(total > 80_000, "样本量应接近 10 万，实际 {total}");
    let expected = total as f64 / N as f64;
    let band = expected * 0.05;
    for (i, &c) in counts.iter().enumerate() {
        let dev = (c as f64 - expected).abs();
        assert!(
            dev < band,
            "成员 {i} 出现 {c} 次，期望 {expected:.1}，偏差 {dev:.1} 超过 5%"
        );
    }
}

// ===========================================================================
// 7. 随机源与可复现性
// ===========================================================================

#[test]
fn same_seed_reproduces_the_same_round_order() {
    let members = range(50);
    let mut a = RoundSampler::new();
    let mut b = RoundSampler::new();
    a.reset(&members);
    b.reset(&members);

    let mut rng_a = Rng::new(0xAAAA_BBBB_CCCC_DDDD);
    let mut rng_b = Rng::new(0xAAAA_BBBB_CCCC_DDDD);
    assert_eq!(a.draw(20, &mut rng_a), b.draw(20, &mut rng_b));
    assert_eq!(a.draw(20, &mut rng_a), b.draw(20, &mut rng_b));
    assert_eq!(a.remaining(), b.remaining());
}

#[test]
fn different_seeds_produce_different_orders() {
    let mut a = RoundSampler::new();
    let mut b = RoundSampler::new();
    a.reset(&range(50));
    b.reset(&range(50));
    let mut rng_a = Rng::new(1);
    let mut rng_b = Rng::new(2);
    let seq_a = a.draw(10, &mut rng_a);
    let seq_b = b.draw(10, &mut rng_b);
    assert_ne!(seq_a, seq_b, "不同种子不应给出同一抽取顺序");
}

#[test]
fn rng_basics_are_public_and_in_range() {
    let mut rng = Rng::new(99);
    assert_eq!(rng.next_below(1), 0, "[0,1) 里只有 0");
    for n in 1..=97usize {
        for _ in 0..50 {
            assert!(rng.next_below(n) < n);
        }
    }
    // 相同种子 -> 相同序列（可复现性）
    let mut x = Rng::new(7);
    let mut y = Rng::new(7);
    for _ in 0..64 {
        assert_eq!(x.next_u64(), y.next_u64());
    }
}

// ===========================================================================
// 8. 规模与稳定性
// ===========================================================================

#[test]
fn large_round_covers_every_member() {
    const N: u32 = 1000;
    let members = range(N);
    let mut sampler = RoundSampler::new();
    sampler.reset(&members);

    let mut rng = Rng::new(0x9876_5432_10FE_DCBA);
    let mut seen = BTreeSet::new();
    while sampler.remaining() > 0 {
        for idx in sampler.draw(7, &mut rng) {
            assert!(seen.insert(idx), "大轮次内出现重复：{idx}");
        }
    }
    assert_eq!(seen.len(), N as usize);
    assert_eq!(seen, set_of(&members));
}

#[test]
fn sampling_with_extreme_k_is_bounded() {
    let members = range(8);
    let mut sampler = RoundSampler::new();
    sampler.reset(&members);
    let mut rng = Rng::new(3);

    // k = usize::MAX 不能溢出，只能给满本轮剩余。
    let out = sampler.draw(usize::MAX, &mut rng);
    assert_eq!(out.len(), 8);
    assert_eq!(sampler.remaining(), 0);
    assert_eq!(set_of(&out), set_of(&members));
}
