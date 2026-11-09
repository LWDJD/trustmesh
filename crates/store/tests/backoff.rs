//! 集成测试 E1：连败计数、live 判定、退避抽样与兜底探测（设计规格 4.9）。
//!
//! 只通过公开 API 验证四件事：
//!
//! 1. 连败计数只由失败累加、只由成功清零；失败不改 last_check，也不改评分。
//! 2. is_live 的判定式与 4.9.3 一致（严格小于，max_age = 0 时全部剔除）。
//! 3. draw_backoff 按 1/2^连败 的相对权重不放回抽样，且「连败全 0」时
//!    与原来的 draw 逐位一致（旧行为不变）。
//! 4. draw_all 一次取走本轮剩下的全部候选，完全不看退避（兜底探测）。
//!
//! 所有随机性都由固定种子的 Rng 驱动，统计检验的断言带足够宽的置信带，
//! 因此本文件是确定性的，不会出现「偶尔红一次」。

use std::collections::BTreeSet;

use tm_store::{AccountIdx, PositiveList, Rng, RoundSampler, Score, Time};

/// 构造 [0, n) 的连续编号。
fn members(n: u32) -> Vec<AccountIdx> {
    (0..n).collect()
}

/// 构造一份正分清单，所有条目的评分与上次检查时间都相同。
fn list_of(ids: &[AccountIdx], score: Score, now: Time) -> PositiveList {
    let mut list = PositiveList::new();
    for &id in ids {
        list.set(id, score, now);
    }
    list
}

/// 把切片收成集合，用来断言「集合相等」而不关心顺序。
fn set_of(v: &[AccountIdx]) -> BTreeSet<AccountIdx> {
    v.iter().copied().collect()
}

// ===========================================================================
// 1. 连败计数：失败 +1、成功清零
// ===========================================================================

#[test]
fn fail_streak_starts_at_zero_and_accumulates() {
    let mut list = PositiveList::new();
    list.set(7, 0.5, 1000);

    // 新条目、从未失败：0
    assert_eq!(list.fail_streak(7), 0);

    // 每次失败都返回新的连续失败次数
    for expected in 1..=10u32 {
        assert_eq!(list.record_failure(7), expected, "第 {expected} 次失败");
        assert_eq!(list.fail_streak(7), expected);
    }
    assert_eq!(list.len(), 1, "失败不会增删条目");
}

#[test]
fn failure_on_unknown_entry_is_a_noop() {
    let mut list = PositiveList::new();
    assert_eq!(list.record_failure(99), 0, "无此条目：没有可累加的连败");
    assert!(!list.has(99), "失败不得凭空创建条目");
    assert_eq!(list.len(), 0);
    assert_eq!(list.fail_streak(99), 0);

    // 删除后连败随条目一起消失，再失败也不会有残留计数
    list.set(1, 0.5, 10);
    assert_eq!(list.record_failure(1), 1);
    list.set(1, 0.0, 20);
    assert_eq!(list.fail_streak(1), 0, "条目被删，连败也该消失");
    assert_eq!(list.record_failure(1), 0);
}

#[test]
fn success_clears_streak_and_refreshes_last_check() {
    let mut list = PositiveList::new();
    list.set(3, 0.6, 100);
    for _ in 0..7 {
        list.record_failure(3);
    }
    assert_eq!(list.fail_streak(3), 7);

    list.record_success(3, 500);
    assert_eq!(list.fail_streak(3), 0, "成功必须清零");
    assert_eq!(list.last_check(3), Some(500), "成功必须刷新 last_check");
    assert_eq!(list.get(3), Some(0.6), "成功不改评分");

    // 清零之后重新从 1 开始累加
    assert_eq!(list.record_failure(3), 1);
}

#[test]
fn success_on_unknown_entry_is_a_noop() {
    let mut list = PositiveList::new();
    list.record_success(42, 999);
    assert!(!list.has(42), "成功不得凭空创建条目");
    assert_eq!(list.len(), 0);
    assert_eq!(list.fail_streak(42), 0);
    assert_eq!(list.last_check(42), None);

    // 删除之后成功也是空操作
    list.set(1, 0.5, 10);
    list.set(1, 0.0, 20);
    list.record_success(1, 30);
    assert!(!list.has(1));
    assert_eq!(list.last_check(1), None);
}

#[test]
fn failure_does_not_touch_last_check_or_score() {
    let mut list = PositiveList::new();
    list.set(6, 0.75, 1000);
    for _ in 0..20 {
        list.record_failure(6);
    }

    assert_eq!(list.last_check(6), Some(1000), "失败不刷新 last_check");
    assert_eq!(list.get(6), Some(0.75), "失败不改评分");
    assert_eq!(list.len(), 1, "失败不增删条目");
    // 因此 age 会自然增长：7 天后该来源自动退出 live 集合
    assert!(!list.is_live(6, 1000 + 168, 168));
}

#[test]
fn set_keeps_streak_but_removal_clears_it() {
    let mut list = PositiveList::new();
    list.set(4, 0.2, 100);
    assert_eq!(list.record_failure(4), 1);
    assert_eq!(list.record_failure(4), 2);

    // 改分不是「取到了观点」的证据：连败保留，时间照旧刷新
    list.set(4, 0.9, 200);
    assert_eq!(list.fail_streak(4), 2);
    assert_eq!(list.last_check(4), Some(200));
    assert_eq!(list.get(4), Some(0.9));

    // 撤销评分（score = 0）把条目连同连败一起删掉
    list.set(4, 0.0, 300);
    assert_eq!(list.fail_streak(4), 0);
    assert!(!list.has(4));
}

// ===========================================================================
// 2. live 集合：退出与恢复
// ===========================================================================

#[test]
fn is_live_boundary_cases() {
    const T_REMOVE: Time = 168;
    let mut list = PositiveList::new();
    list.set(1, 0.5, 1000);

    // age = 0 / 1 / 167 / 168 / 169
    assert!(list.is_live(1, 1000, T_REMOVE));
    assert!(list.is_live(1, 1001, T_REMOVE));
    assert!(list.is_live(1, 1167, T_REMOVE));
    assert!(!list.is_live(1, 1168, T_REMOVE), "age == max_age 不算 live");
    assert!(!list.is_live(1, 1169, T_REMOVE));

    // 条目不存在：一律 false
    assert!(!list.is_live(2, 1000, T_REMOVE));
    assert!(!list.is_live(2, 0, Time::MAX));

    // max_age = 0：全部剔除（age < 0 不可能成立），不 panic
    assert!(!list.is_live(1, 1000, 0));
    assert!(!list.is_live(1, 0, 0));

    // 时间回拨按 age = 0 处理，仍算 live
    assert!(list.is_live(1, 0, T_REMOVE));

    // max_age = u64::MAX：只要条目存在且时间不前移就 live
    assert!(list.is_live(1, Time::MAX, Time::MAX));
}

#[test]
fn live_recovers_immediately_after_success() {
    const T_REMOVE: Time = 168;
    let mut list = PositiveList::new();
    list.set(1, 0.5, 0);
    for _ in 0..4 {
        list.record_failure(1);
    }

    // 7 天后退出 live 集合，但个人评分与连败计数都还在（剔除不是删除）
    let now = 200;
    assert!(!list.is_live(1, now, T_REMOVE));
    assert_eq!(list.get(1), Some(0.5));
    assert_eq!(list.fail_streak(1), 4);

    // 一次成功刷新：age 归零 + 连败清零，立即重回 live
    list.record_success(1, now);
    assert!(list.is_live(1, now, T_REMOVE));
    assert_eq!(list.fail_streak(1), 0);
    assert_eq!(list.len(), 1);
}

// ===========================================================================
// 3. 退避抽样：权重、不可重复、与旧路径一致
// ===========================================================================

#[test]
fn draw_backoff_matches_draw_when_all_streaks_zero() {
    // 旧行为不变的硬约束：连败全 0 时，draw_backoff 与 draw 必须逐位相同。
    let ids = members(10);
    let list = list_of(&ids, 0.5, 1000);

    let mut plain = RoundSampler::new();
    let mut backoff = RoundSampler::new();
    plain.reset(&ids);
    backoff.reset(&ids);

    let mut rng_plain = Rng::new(0xC0FF_EE00_1234_5678);
    let mut rng_backoff = Rng::new(0xC0FF_EE00_1234_5678);

    for k in [1usize, 3, 7, 2, 5, 10, 4] {
        assert_eq!(
            plain.draw(k, &mut rng_plain),
            backoff.draw_backoff(&list, k, &mut rng_backoff),
            "k = {k} 时两条路径分叉了"
        );
        assert_eq!(plain.remaining(), backoff.remaining());
    }
}

#[test]
fn draw_backoff_matches_draw_when_all_streaks_equal() {
    // 连败全为 3：相对权重全为 1，同样必须与 draw 逐位一致
    let ids = members(10);
    let mut list = list_of(&ids, 0.5, 1000);
    for &id in &ids {
        for _ in 0..3 {
            list.record_failure(id);
        }
    }

    let mut plain = RoundSampler::new();
    let mut backoff = RoundSampler::new();
    plain.reset(&ids);
    backoff.reset(&ids);
    let mut rng_plain = Rng::new(0xAAA_BBBB);
    let mut rng_backoff = Rng::new(0xAAA_BBBB);

    for k in [2usize, 4, 1, 9, 3] {
        assert_eq!(
            plain.draw(k, &mut rng_plain),
            backoff.draw_backoff(&list, k, &mut rng_backoff),
            "k = {k} 时两条路径分叉了"
        );
        assert_eq!(plain.remaining(), backoff.remaining());
    }
}

#[test]
fn draw_backoff_never_repeats_within_a_round() {
    let ids = members(9);
    let mut list = list_of(&ids, 0.5, 1000);
    for _ in 0..2 {
        list.record_failure(3);
    }
    for _ in 0..5 {
        list.record_failure(7);
    }

    let mut sampler = RoundSampler::new();
    sampler.reset(&ids);
    let mut rng = Rng::new(0xDEAD_BEEF);
    let mut seen = BTreeSet::new();
    while sampler.remaining() > 0 {
        for id in sampler.draw_backoff(&list, 2, &mut rng) {
            assert!(seen.insert(id), "同一轮内重复抽到 {id}");
        }
    }
    assert_eq!(seen, set_of(&ids), "一轮必须恰好覆盖全部成员");
}

#[test]
fn draw_backoff_auto_resets_after_full_round() {
    let ids = members(5);
    let mut list = list_of(&ids, 0.5, 1000);
    for _ in 0..9 {
        list.record_failure(1);
    }

    let mut sampler = RoundSampler::new();
    sampler.reset(&ids);
    let mut rng = Rng::new(0x5EED_5EED);
    let full = sampler.draw_backoff(&list, 5, &mut rng);
    assert_eq!(full.len(), 5);
    assert_eq!(set_of(&full), set_of(&ids));
    assert_eq!(sampler.remaining(), 0);

    // 本轮位图已满：下一次调用自动开新一轮，仍能再抽到（包括连败 9 次的那个）
    let next = sampler.draw_backoff(&list, 5, &mut rng);
    assert_eq!(next.len(), 5);
    assert_eq!(sampler.remaining(), 0);
    assert!(next.contains(&1), "新一轮必须能再抽到长期失败的来源");
}

#[test]
fn backoff_reduces_pick_frequency_for_failing_source() {
    // 10 个候选，只有 0 号连败 2 次（权重 1/4）。
    // 理论首抽概率：p0 = 0.25 / 9.25，p_other = 1 / 9.25。
    const ROUNDS: usize = 6_000;
    let ids = members(10);
    let mut list = list_of(&ids, 0.5, 1000);
    assert_eq!(list.record_failure(0), 1);
    assert_eq!(list.record_failure(0), 2);

    let mut sampler = RoundSampler::new();
    let mut rng = Rng::new(0xE120_24BA_CC0F_F000);
    let mut counts = [0usize; 10];
    for _ in 0..ROUNDS {
        sampler.reset(&ids);
        let picked = sampler.draw_backoff(&list, 1, &mut rng);
        assert_eq!(picked.len(), 1);
        counts[picked[0] as usize] += 1;
    }

    let total_weight = 9.0 + 0.25;
    let expected_slow = ROUNDS as f64 * (0.25 / total_weight);
    let expected_fast = ROUNDS as f64 * (1.0 / total_weight);

    let observed_slow = counts[0] as f64;
    assert!(
        observed_slow > expected_slow * 0.5 && observed_slow < expected_slow * 1.5,
        "连败 2 次的来源出现 {observed_slow} 次，理论 {expected_slow:.1} 次"
    );
    // 相对均匀抽样（每次 1/10）应当明显下降
    assert!(
        observed_slow < ROUNDS as f64 * 0.05,
        "退避没有生效：{observed_slow} 次，均匀期望 {} 次",
        ROUNDS / 10
    );
    for (i, &c) in counts.iter().enumerate().skip(1) {
        let dev = (c as f64 - expected_fast).abs();
        assert!(
            dev < expected_fast * 0.15,
            "正常来源 {i} 出现 {c} 次，理论 {expected_fast:.1}，偏差过大"
        );
    }
    assert_eq!(counts.iter().sum::<usize>(), ROUNDS);
}

#[test]
fn backoff_frequency_matches_one_over_two_pow_streak() {
    // 3 个候选，连败 0 / 1 / 2 -> 权重 1 / 1/2 / 1/4 -> 概率 4/7、2/7、1/7。
    const ROUNDS: usize = 7_000;
    let ids = members(3);
    let mut list = list_of(&ids, 0.5, 0);
    list.record_failure(1);
    list.record_failure(2);
    list.record_failure(2);

    let mut sampler = RoundSampler::new();
    let mut rng = Rng::new(0x0BAD_F00D_1234_5678);
    let mut counts = [0usize; 3];
    for _ in 0..ROUNDS {
        sampler.reset(&ids);
        counts[sampler.draw_backoff(&list, 1, &mut rng)[0] as usize] += 1;
    }

    let expected = [4.0 / 7.0, 2.0 / 7.0, 1.0 / 7.0];
    for (i, &p) in expected.iter().enumerate() {
        let want = ROUNDS as f64 * p;
        let got = counts[i] as f64;
        assert!(
            (got - want).abs() < want * 0.12,
            "连败 {i} 次的来源出现 {got} 次，理论 {want:.1} 次"
        );
    }
    assert_eq!(counts.iter().sum::<usize>(), ROUNDS);
}

#[test]
fn backoff_almost_never_picks_source_with_huge_streak() {
    // 5 个候选，0 号连败 40 次 -> 权重 2^-40（约 9.1e-13）。
    // 理论首抽概率约 2.3e-13，1 万次里几乎必然一次都抽不到。
    const ROUNDS: usize = 10_000;
    let ids = members(5);
    let mut list = list_of(&ids, 0.5, 0);
    for _ in 0..40 {
        list.record_failure(0);
    }

    let mut sampler = RoundSampler::new();
    let mut rng = Rng::new(0xFEED_0BAD_C0DE_0001);
    let mut counts = [0usize; 5];
    for _ in 0..ROUNDS {
        sampler.reset(&ids);
        counts[sampler.draw_backoff(&list, 1, &mut rng)[0] as usize] += 1;
    }

    assert_eq!(counts[0], 0, "权重 2^-40 的来源几乎不应出现在首抽里");
    let expected = ROUNDS as f64 / 4.0;
    for (i, &c) in counts.iter().enumerate().skip(1) {
        let dev = (c as f64 - expected).abs();
        assert!(
            dev < expected * 0.08,
            "来源 {i} 出现 {c} 次，理论 {expected:.1} 次"
        );
    }
    assert_eq!(counts.iter().sum::<usize>(), ROUNDS);
}

#[test]
fn backoff_all_extreme_weights_still_fills_k() {
    // 每个候选都连败 800 次：绝对权重 2^-800 在 f64 里是 0，
    // 但相对权重全部相等，必须照常抽满 k 个。
    let ids = members(8);
    let mut flat = list_of(&ids, 0.5, 0);
    for &id in &ids {
        for _ in 0..800 {
            flat.record_failure(id);
        }
    }
    assert_eq!(flat.fail_streak(0), 800);

    // 差值远超 f64 能表示的范围：相对权重仍然有意义（被截断到 2^-64）
    let mut skewed = list_of(&ids, 0.5, 0);
    for _ in 0..800 {
        skewed.record_failure(0);
    }
    for id in 1..8u32 {
        for _ in 0..100 {
            skewed.record_failure(id);
        }
    }

    let mut sampler = RoundSampler::new();
    let mut rng = Rng::new(0x5EED_1234_5678_9ABC);
    for list in [&flat, &skewed] {
        sampler.reset(&ids);
        let picked = sampler.draw_backoff(list, 3, &mut rng);
        assert_eq!(picked.len(), 3, "权重再小也必须抽满 k 个");
        assert_eq!(set_of(&picked).len(), 3, "不得重复");
        assert!(picked.iter().all(|i| ids.contains(i)));

        let rest = sampler.draw_backoff(list, usize::MAX, &mut rng);
        assert_eq!(rest.len(), 5, "剩下的必须全部给出");
        assert_eq!(sampler.remaining(), 0);

        let all: BTreeSet<AccountIdx> = picked.iter().chain(rest.iter()).copied().collect();
        assert_eq!(all, set_of(&ids), "一轮必须覆盖全部候选");
    }
}

#[test]
fn draw_backoff_is_deterministic_for_fixed_seed() {
    // 脚本化的一次「多轮抽样」：相同 seed 必须给出完全相同的序列。
    fn script() -> Vec<Vec<AccountIdx>> {
        let ids = members(12);
        let mut list = list_of(&ids, 0.5, 1000);
        for _ in 0..3 {
            list.record_failure(2);
        }
        list.record_failure(5);

        let mut sampler = RoundSampler::new();
        sampler.reset(&ids);
        let mut rng = Rng::new(0xE1E1_E1E1_2024_2024);
        let mut out = Vec::new();
        for k in [1usize, 2, 5, 4, 1, 3] {
            out.push(sampler.draw_backoff(&list, k, &mut rng));
        }
        out.push(sampler.draw_all(&list));
        out
    }

    assert_eq!(script(), script(), "同 seed 必须逐位一致");
}

#[test]
fn k_zero_does_not_consume_round() {
    let ids = members(4);
    let list = list_of(&ids, 0.5, 0);
    let mut sampler = RoundSampler::new();
    sampler.reset(&ids);
    let mut rng = Rng::new(3);

    assert!(sampler.draw_backoff(&list, 0, &mut rng).is_empty());
    assert_eq!(sampler.remaining(), 4, "k = 0 不得消耗名额");
    assert!(sampler.draw_backoff(&list, 0, &mut rng).is_empty());

    // 抽空之后 k = 0 也不得触发重置
    assert_eq!(sampler.draw_backoff(&list, 4, &mut rng).len(), 4);
    assert_eq!(sampler.remaining(), 0);
    assert!(sampler.draw_backoff(&list, 0, &mut rng).is_empty());
    assert_eq!(sampler.remaining(), 0, "k = 0 不得触发自动重置");
}

// ===========================================================================
// 4. 兜底探测：draw_all
// ===========================================================================

#[test]
fn draw_all_returns_every_undrawn_entry() {
    let ids = members(6);
    let list = list_of(&ids, 0.5, 1000);
    let mut sampler = RoundSampler::new();
    sampler.reset(&ids);
    let mut rng = Rng::new(0x1234_ABCD);

    let drawn = sampler.draw_backoff(&list, 2, &mut rng);
    assert_eq!(drawn.len(), 2);
    assert_eq!(sampler.remaining(), 4);

    let probe = sampler.draw_all(&list);
    assert_eq!(probe.len(), 4, "必须返回本轮全部未抽过条目");
    assert_eq!(sampler.remaining(), 0, "探测之后本轮剩余清零");
    assert!(probe.windows(2).all(|w| w[0] < w[1]), "结果必须升序");
    assert!(drawn.iter().all(|i| !probe.contains(i)), "不得重复已抽过的");

    let all: BTreeSet<AccountIdx> = drawn.iter().chain(probe.iter()).copied().collect();
    assert_eq!(all, set_of(&ids));

    // 本轮已空：与 draw 一样自动开新一轮，探测不会「什么也没问」
    let again = sampler.draw_all(&list);
    assert_eq!(set_of(&again), set_of(&ids));
}

#[test]
fn draw_all_ignores_fail_streak() {
    let ids = members(5);
    let mut list = list_of(&ids, 0.5, 1000);
    for &id in &ids {
        for _ in 0..100 {
            list.record_failure(id);
        }
    }

    let mut sampler = RoundSampler::new();
    sampler.reset(&ids);
    let probe = sampler.draw_all(&list);
    assert_eq!(set_of(&probe), set_of(&ids), "连败再大也照样返回");
    assert_eq!(sampler.remaining(), 0);
    assert!(probe.windows(2).all(|w| w[0] < w[1]), "结果必须升序");
}

#[test]
fn draw_all_works_after_sync_without_reset() {
    // 实际调用顺序：sync(全量) -> （可能还没抽过）-> 24h 到点，兜底探测。
    // sync 只记录成员、不主动开轮，draw_all 必须自己开一轮，不能返回空。
    let ids = members(3);
    let list = list_of(&ids, 0.5, 0);
    let mut sampler = RoundSampler::new();
    sampler.sync(&list.indices());
    assert_eq!(sampler.remaining(), 0);

    let probe = sampler.draw_all(&list);
    assert_eq!(
        set_of(&probe),
        set_of(&ids),
        "只要清单非空，探测必然覆盖一轮"
    );
}

#[test]
fn backoff_treats_unknown_indices_as_zero_streak() {
    // 调用方若忘了 sync，本轮可能残留已经从清单里删掉的编号。
    // 约定的兜底行为是「查不到就连败 0 次」，而不是跳过（不得少抽）。
    let ids = members(3);
    let list = list_of(&ids, 0.5, 0);
    let stale = 99;
    let mut sampler = RoundSampler::new();
    sampler.reset(&[0, 1, 2, stale]);

    let mut rng = Rng::new(0x99);
    let picked = sampler.draw_backoff(&list, 4, &mut rng);
    assert_eq!(picked.len(), 4, "残留编号不得让本轮少抽");
    assert_eq!(set_of(&picked), set_of(&[0, 1, 2, stale]));
}

#[test]
fn e2e_backoff_probe_recovery() {
    // 场景：3 个来源，1 号长期离线。
    const T_REMOVE: Time = 168;
    let ids = members(3);
    let mut list = list_of(&ids, 0.5, 0);
    for _ in 0..6 {
        list.record_failure(1);
    }

    let now = 500;
    assert_eq!(list.fail_streak(1), 6);
    assert_eq!(list.last_check(1), Some(0), "失败不刷新时间");
    assert!(!list.is_live(1, now, T_REMOVE), "超过 T_remove 应退出 live");
    assert_eq!(list.get(1), Some(0.5), "个人评分照常保留");

    // 兜底探测不看退避，1 号仍会被问到
    let mut sampler = RoundSampler::new();
    sampler.reset(&ids);
    let probe = sampler.draw_all(&list);
    assert_eq!(probe.len(), 3);
    assert!(probe.contains(&1), "兜底探测必须覆盖长期离线的来源");

    // 探测成功：age 归零、连败清零，立即回到 live 集合
    list.record_success(1, now);
    assert!(list.is_live(1, now, T_REMOVE));
    assert_eq!(list.fail_streak(1), 0);
}

// ===========================================================================
// 5. 边界：空清单、k = 0、k 过大
// ===========================================================================

#[test]
fn degenerate_inputs_are_safe() {
    let empty = PositiveList::new();
    let mut sampler = RoundSampler::new();
    let mut rng = Rng::new(1);

    // 空清单：抽不出东西，但也不能 panic
    assert!(sampler.draw_backoff(&empty, 3, &mut rng).is_empty());
    assert!(sampler
        .draw_backoff(&empty, usize::MAX, &mut rng)
        .is_empty());
    assert!(sampler.draw_all(&empty).is_empty());
    assert_eq!(sampler.remaining(), 0);

    // 空清单（从未同步过成员）上的兜底探测同样安全
    let mut fresh = RoundSampler::new();
    assert!(fresh.draw_all(&empty).is_empty());
    assert_eq!(fresh.remaining(), 0);

    // k = 0：不消耗名额、不触发重置
    let ids = members(3);
    let list = list_of(&ids, 0.5, 0);
    sampler.reset(&ids);
    assert!(sampler.draw_backoff(&list, 0, &mut rng).is_empty());
    assert_eq!(sampler.remaining(), 3, "k = 0 不得消耗本轮名额");

    // k 大于清单长度：只给剩下的，且不重复
    let got = sampler.draw_backoff(&list, 100, &mut rng);
    assert_eq!(got.len(), 3);
    assert_eq!(set_of(&got), set_of(&ids));
    assert_eq!(sampler.remaining(), 0);

    // 抽空之后 k = usize::MAX：自动开新一轮，不会溢出/panic
    let again = sampler.draw_backoff(&list, usize::MAX, &mut rng);
    assert_eq!(again.len(), 3);
    assert_eq!(set_of(&again), set_of(&ids));
}
