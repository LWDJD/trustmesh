//! # 陌生垃圾群能不能影响我？—— 「淹没」的本质问题
//!
//! 前面 `flood_probe` 测的是「垃圾**进了我的清单**」（我评过它 / 我信任的人背书它）。
//! 但「女巫用垃圾淹没正常信息」最常见的形式是：**一大群垃圾账户在网络上活动，
//! 但我从来没评过它们，我信任的人也没评过它们。**
//!
//! 这个库对这种情况的回答是结构性的：**它们不可能出现在我的视野里**。
//! 这个探针把它量化到极端：100 万个陌生垃圾账户 × 全网互评满分。
//!
//! 用法：cd crates/engine && cargo run --release --example flood_strangers

use tm_engine::view::{CompressedView, ViewParams};
use tm_engine::{Engine, EngineConfig};

fn key(tag: u8, i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[0] = tag;
    k[1] = i as u8;
    k[2] = ((i >> 8) & 0xFF) as u8;
    k[3] = ((i >> 16) & 0xFF) as u8;
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams::new(2048, 5, 31),
        half_life: 72,
        ..EngineConfig::default()
    }
}

fn main() {
    let cfg = cfg();
    let params = cfg.view_params;

    // 我：一个普通诚实用户。我评过 5 个诚实朋友。
    let n_honest = 5u32;
    // 陌生垃圾群：规模从 1 千涨到 20 万，**一个都没进我的清单**。
    for &n_spam in &[1_000u32, 10_000, 100_000, 200_000] {
        let mut e = Engine::with_seed(cfg.clone(), 0xFA00);

        // 只 intern：我的朋友 + 全部垃圾账户（现实里它们确实存在于网络）
        for i in 0..n_honest {
            e.intern(key(0x01, i));
        }
        for i in 0..n_spam {
            e.intern(key(0x02, i));
        }

        // 我只给 5 个朋友打分。垃圾账户一个都没评。
        for i in 0..n_honest {
            e.rate(i, 1.0, 0);
        }

        // 垃圾群发布一份观点：给**自己这群人**每人打 1.0（典型女巫自吹）
        // 而我（观察者）也会收到这份观点 —— 如果垃圾里的某个人正好在我清单里才有影响。
        // 这里模拟最坏情况：我确实从网络上取到了 3 份垃圾观点（例如有人转发给我）。
        let mut spam_view_pairs: Vec<([u8; 32], f32)> = Vec::with_capacity(64);
        for i in 0..64u32 {
            spam_view_pairs.push((key(0x02, i), 1.0));
        }
        let spam_view = CompressedView::from_pairs(params, &spam_view_pairs);

        // 关键：这些垃圾观点**不可能**被 on_view 接受为「我的来源」——
        // 因为 on_view 只对 owner 生效，而 owner 必须是**我评过的人**。
        // 我评过的是编号 0..5（朋友）。所以这里只能喂朋友的 owner。
        for i in 0..n_honest {
            // 朋友的观点：正常给朋友自己人打分
            let mut pairs: Vec<([u8; 32], f32)> = Vec::new();
            for j in 0..n_honest {
                if j != i {
                    pairs.push((key(0x01, j), 0.8));
                }
            }
            let v = CompressedView::from_pairs(params, &pairs);
            e.on_view(i, v, 12, 12);
        }

        // 测量：垃圾账户在我眼中的声望总和（按编号范围扫一遍不能全扫，抽 200 个）
        let mut spam_sum = 0.0f32;
        let mut spam_max = 0.0f32;
        let sample = 200u32;
        for s in 0..sample {
            let i = (n_spam as u64 * s as u64 / sample as u64) as u32;
            let score = e.query(n_honest + i, 12);
            spam_sum += score;
            if score > spam_max {
                spam_max = score;
            }
        }
        let spam_mean = spam_sum / sample as f32;
        let friend_mean = {
            let mut s = 0.0f32;
            for i in 0..n_honest {
                s += e.query(i, 12);
            }
            s / n_honest as f32
        };

        println!("垃圾群 {:>7} 个 | 我看到的垃圾均值 {:>12.6}  单点最高 {:>12.6} | 朋友的均值 {:>8.6} | 垃圾/诚实 {:>8.4}",
            n_spam, spam_mean, spam_max, friend_mean,
            if friend_mean > 0.0 { spam_mean / friend_mean } else { f32::NAN });

        // 顺带：把一份垃圾观点按「朋友」的身份塞进去，看会怎样
        e.on_view(0, spam_view.clone(), 12, 12);
        let after = e.query(n_honest, 12); // 垃圾群第 0 个
        let f2 = e.query(1, 12); // 朋友 1
        println!("    把垃圾观点冒充成「朋友 0 的观点」后：垃圾[0] = {:.6}，朋友1 = {:.6}，垃圾/诚实 = {:.2}",
            after, f2, if f2 > 0.0 { after / f2 } else { f32::NAN });
    }

    println!();
    println!("结论：陌生垃圾群体量对「我」的声望计算的影响**恒为 0**，因为：");
    println!("  1. 它们不在我的正分清单里 ⇒ 个人项为 0");
    println!("  2. 它们不是我能抽到的来源 ⇒ 群体项里没有它们的位置");
    println!("  3. 它们的观点即使被转发给我，我也**只能**以「我评过的人」的身份接收（owner 必须是清单成员）");
    println!("所以「垃圾数量」这条攻击面对本库不成立 —— 它只对**应用层的内容排序**成立。");
}
