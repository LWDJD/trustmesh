//! # 无限规模的第一道墙：`AccountIdx` 是**局部编号**，但 `mass_covers` 被跨节点比较了。
//!
//! ## 协议要求（`PROTOCOL.md` 不变量 1）
//!
//! > **账户编号顺序全网一致**。编号是 `intern` 的先后次序。两个节点对同一批公钥的
//! > 编号不同 ⇒ ... `mass_covers` 这个「已知账户数」上界会错 ⇒ 后登记账户的白送背书防护失效
//!
//! ## 为什么这条在无限网络上**不可能满足**
//!
//! 每个节点 `intern` 的是**它自己见过的账户**。在无限网络上，节点 A 见过 {a,b,c}，
//! 节点 B 见过 {a,b,c,d,e} —— 两者的编号空间是**两套不同的坐标系**：
//!
//! ```text
//! 节点 A 的编号：a=0, b=1, c=2                       （len = 3）
//! 节点 B 的编号：a=0, b=1, c=2, d=3, e=4             （len = 5）
//! ```
//!
//! 现在 B 发布一份观点，`mass_covers = 5`。A 收到后，检查某个账户 `idx` 是不是
//! 在 `0..5` 范围内——但 `idx` 是 **A 的坐标系**里的编号，`5` 是 **B 的坐标系**里的长度。
//!
//! **两个不同坐标系的数量被直接比较了。**
//!
//! 后果分两种方向：
//!
//! * **A 的编号空间更小**（A 只认识 {a,b,c}，len=3）：B 说 `mass_covers=5`，于是 A 认为
//!   「0..5 都在 B 的视图里」，但 A 的 idx 只到 2 —— 这个方向**恰好安全**（宽松）；
//! * **A 的编号空间更大**（A 认识 {a,b,c,d,e,f,g}，len=7）：B 说 `mass_covers=5`，于是 A 认为
//!   「idx >= 5 的账户 B 没表态」。可 A 的 idx=5 是 `f`，而 B 的视图里**可能真的包含 `f`**
//!   （因为 B 也可以知道 `f`，只是它 intern 的时机不同，编号不同）。
//!
//! **第二方向就是白送背书漏洞的复活**：一个账户被误判为「B 没表态」，于是 B 的群体项
//! 不给它任何约束，它也就不会被群体项压低。
//!
//! 用法：cd crates/engine && cargo run --release --example unbounded_index

use tm_engine::view::ViewParams;
use tm_engine::{Engine, EngineConfig};

fn key(name: &str) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..name.len()].copy_from_slice(name.as_bytes());
    k
}

fn cfg() -> EngineConfig {
    EngineConfig {
        view_params: ViewParams::new(2048, 5, 3),
        ..EngineConfig::default()
    }
}

fn main() {
    println!("# 无限规模第 1 道墙：局部编号 vs 跨节点 mass_covers");
    println!();

    // 两个节点，认识**不同数量**的账户。这是无限网络里最普通的情况。
    // A = 小节点（只走少数朋友）
    let a_known = ["alice", "bob", "carol"];
    // B = 大节点（认识更多，包括 A 不认识的 dave/frank）
    let b_known = ["alice", "bob", "carol", "dave", "frank", "grace", "heidi"];

    let mut ea = Engine::with_seed(cfg(), 0xA1);
    for n in a_known {
        ea.intern(key(n));
    }

    let mut eb = Engine::with_seed(cfg(), 0xB1);
    // 关键：B 的 intern 顺序与 A **不同**（现实中几乎必然不同）
    for n in b_known {
        eb.intern(key(n));
    }

    println!(
        "节点 A 认识 {} 个账户，编号 0..{}",
        a_known.len(),
        a_known.len() - 1
    );
    println!(
        "节点 B 认识 {} 个账户，编号 0..{}",
        b_known.len(),
        b_known.len() - 1
    );
    println!();
    println!("A 的编号表：");
    for n in a_known {
        println!("  {:?} = {}", n, ea.lookup(&key(n)).unwrap());
    }
    println!("B 的编号表：");
    for n in b_known {
        println!("  {:?} = {}", n, eb.lookup(&key(n)).unwrap());
    }

    // B 给 grace 打高分并发布观点（grace 对 A 来说是**陌生账户**）
    let grace_b = eb.lookup(&key("grace")).unwrap();
    eb.rate(grace_b, 1.0, 0);
    let view = eb.export_view(12);
    println!();
    println!(
        "B 发布观点的 mass_covers = {}（B 的坐标系：它有 7 个账户）",
        eb.account_count()
    );

    // A 也把 grace intern 进来（因为它从网络上听说了 grace）
    let grace_a = ea.intern(key("grace"));
    println!(
        "A 现在把 grace 登记为编号 {}（A 的坐标系：A 有 {} 个账户）",
        grace_a,
        ea.account_count()
    );

    // A 收到 B 的观点（owner = dave，A 也认识 dave）
    let dave_a = ea.intern(key("dave"));
    ea.rate(dave_a, 0.8, 0);
    ea.on_view(dave_a, view.clone(), 12, 12);

    let q = ea.query(grace_a, 12);
    println!();
    println!("A 从 B 的观点里读 grace 得到的声望 = {:.6}", q);
    println!();
    println!("## 分析");
    println!(
        "B 的 mass_covers = 7（B 有 7 个账户）。A 收到时 A 自己有 {} 个账户。",
        ea.account_count()
    );
    println!(
        "A 判断 \"grace 是否在 B 的表态范围内\" 用的是：grace 的 A 编号 {} < 7",
        grace_a
    );
    if (grace_a as usize) < eb.account_count() {
        println!("  → 判定为「在范围内」，于是去读桶值");
    } else {
        println!("  → 判定为「不在范围内」，按「B 没表态」处理（读数被丢弃）");
    }
    println!();
    println!("这行代码是错的：`grace 的 A 编号 < B 的账户数` 把两套坐标系混在一起比较。");
    println!("  正确的问题应该是「B 的视图里有没有 grace 这个 **key**」，而不是「编号比大小」。");
    println!();
    println!("## 结论");
    println!("`mass_covers` 是**局部编号空间的数量**，却被当作**跨节点的语义上界**使用。");
    println!("在小网络、且所有节点 intern 同一批账户时，两套坐标系碰巧一致，所以没暴露。");
    println!("在无限网络上，各节点认识的账户集必然不同 ⇒ 坐标系必然不同 ⇒ 这条比较必然无意义。");
}
