//! # tm_mincore —— 最小反女巫内核（原型）
//!
//! 这不是库的一部分，而是一次**对照实验**：把「去中心化信任网络」砍到只剩
//! 反女巫所必需的四件事，看看砍完之后反女巫是变好还是变差。
//!
//! ```text
//! ① 正分清单       (账户, 正分, 上次成功取到观点的时间, 连败计数)
//! ② 轮次位图抽样   一轮不放回、抽完重置、**不退避**
//! ③ 定长稀疏表     cap 条 × (u32 账户 + u16 定点分数) = cap×6 字节
//! ④ 计算           群体项 = Σ 权重·(分数 ÷ 总质量) ÷ Σ 权重；β = 0.3；全局归一化
//! ```
//!
//! ## 与现行实现的四点差别
//!
//! 1. **观点不再压缩成草图**，而是一张定长稀疏表（账户号 + u16 定点分数）。
//!    `Σ 分数` 因此**精确可算**，接收端不再需要「遍历全部已知账户猜总质量」。
//! 2. **归一化顺序反过来**：先夹到合法区间（u16 定点天然落在 `[0, 1]`），
//!    再除以**声明集合自己的**总质量。于是 `Σv ≡ 1`，一个来源的预算恒为 1，
//!    不存在「每一条都饱和到 1.0」的路径。
//! 3. **不退避**：连败计数只记账、不参与抽样；抽样器是一张轮次位图，
//!    每个来源在一轮内被抽到的概率严格相同。
//! 4. **格式即防御**：u16 定点表达不了 NaN / ∞ / 负数 / 超量，
//!    这些值在入口就被夹紧或丢弃，不需要在接收端逐条判脏。
//!
//! ## 确定性
//!
//! 所有容器都是 `BTreeMap`（按账户号升序），所有浮点累加都走 `f64` 且顺序固定，
//! 序列化按账户号升序、填充区补零。因此同一**集合**的输入，
//! 不论到达顺序如何，输出**逐位相同**（见 `tests/anti_sybil.rs` 的 6 号用例）。
//!
//! ## 例子
//!
//! ```
//! use tm_mincore::{Config, Node, SparseView};
//!
//! let mut me = Node::new(Config::default(), 7);
//! me.rate(1, 0.9, 0);      // 我给 1 号账户 0.9 的正分
//! me.rate(2, 0.3, 0);
//!
//! // 1 号账户发来它的观点：它认为 3 号值 1.0、4 号值 0.5
//! let v = SparseView::from_scores(128, &[(3, 1.0), (4, 0.5)]);
//! assert!(me.on_view(1, v, 0, 0));
//!
//! // 我的声望：3 号拿到 (1−β)·(0.9/1.2)·(1.0/1.5)，再全局归一化
//! let d = me.distribution(0);
//! assert!(d.get(3) > d.get(4));
//! assert!((d.sum() - 1.0).abs() < 1e-6);
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod fixed;
pub mod list;
pub mod node;
pub mod round;
pub mod view;

pub use fixed::{dequantize, quantize, Q_ONE};
pub use list::{Acc, Entry, PositiveList, Time};
pub use node::{decay, Config, Distribution, Node, Received};
pub use round::{Rng, RoundBitmap};
pub use view::{DecodeError, SparseView, MAX_CAP, PREFIX_BYTES, SLOT_BYTES, VIEW_OVERHEAD};
