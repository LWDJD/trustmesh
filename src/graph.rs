//! 图结构与评分矩阵。
//!
//! 邻接权重在 `finalize` 时按行归一化，得到 W。
//! 评分矩阵 P 是稠密的，未设置的条目默认为中性值 `NEUTRAL`。

/// 未评分时的中性值。
pub const NEUTRAL: f64 = 0.5;

pub struct Graph {
    pub n: usize,
    /// 邻接表，已按行归一化。edges[u] = [(v, W[u][v]), ...]
    pub edges: Vec<Vec<(usize, f64)>>
    ,
    /// 评分矩阵，扁平存储，n×n。
    ratings: Vec<f64>,
    /// 归一化前的出边总权重，用于 finalize。
    raw_out: Vec<f64>,
}

impl Graph {
    pub fn new(n: usize) -> Self {
        Self {
            n,
            edges: vec![Vec::new(); n],
            ratings: vec![NEUTRAL; n * n],
            raw_out: vec![0.0; n],
        }
    }

    /// 添加一条有向边 u -> v，权重 w（会被归一化）。
    ///
    /// 同一对 (u,v) 重复添加时，权重累加而不是产生两条边。
    pub fn add_edge(&mut self, u: usize, v: usize, w: f64) {
        assert!(u < self.n && v < self.n);
        assert!(w >= 0.0);
        if w == 0.0 || u == v {
            return;
        }
        if let Some(slot) = self.edges[u].iter_mut().find(|(x, _)| *x == v) {
            slot.1 += w;
        } else {
            self.edges[u].push((v, w));
        }
        self.raw_out[u] += w;
    }

    /// 行归一化。孤立节点的行保持为空，此时该行的 W 全零。
    pub fn finalize(&mut self) {
        for u in 0..self.n {
            let total = self.raw_out[u];
            if total > 0.0 {
                for e in self.edges[u].iter_mut() {
                    e.1 /= total;
                }
            }
        }
    }

    /// 设置 s 对 u 的直接评分，取值应在 [0,1]。
    pub fn set_rating(&mut self, s: usize, u: usize, p: f64) {
        assert!(s < self.n && u < self.n);
        self.ratings[s * self.n + u] = p.clamp(0.0, 1.0);
    }

    pub fn rating(&self, s: usize, u: usize) -> f64 {
        self.ratings[s * self.n + u]
    }

    /// 返回整行评分。
    pub fn rating_row(&self, s: usize) -> &[f64] {
        &self.ratings[s * self.n..(s + 1) * self.n]
    }

    pub fn neighbors(&self, u: usize) -> &[(usize, f64)] {
        &self.edges[u]
    }

    /// 出度（非零权重的边数）。
    pub fn out_degree(&self, u: usize) -> usize {
        self.edges[u].len()
    }

    /// 检查 W 是否行归一（用于自检）。
    pub fn row_sums(&self) -> Vec<f64> {
        self.edges
            .iter()
            .map(|es| es.iter().map(|(_, w)| w).sum())
            .collect()
    }
}
