//! 稠密矩阵与线性方程组求解。
//!
//! 只实现验证所需的最小集合：构造、减法、矩阵乘、高斯消元解线性方程组。
//! N 较小（≤ 200）时足够，复杂度 O(N³)。

#[derive(Clone)]
pub struct Mat {
    pub n: usize,
    pub m: usize,
    pub data: Vec<f64>,
}

impl Mat {
    pub fn zeros(n: usize, m: usize) -> Self {
        Self {
            n,
            m,
            data: vec![0.0; n * m],
        }
    }

    pub fn identity(n: usize) -> Self {
        let mut mat = Self::zeros(n, n);
        for i in 0..n {
            mat.set(i, i, 1.0);
        }
        mat
    }

    #[inline]
    pub fn get(&self, i: usize, j: usize) -> f64 {
        self.data[i * self.m + j]
    }

    #[inline]
    pub fn set(&mut self, i: usize, j: usize, v: f64) {
        self.data[i * self.m + j] = v;
    }

    /// self - other，要求同形。
    pub fn sub(&self, other: &Mat) -> Mat {
        assert_eq!((self.n, self.m), (other.n, other.m));
        let mut out = Self::zeros(self.n, self.m);
        for k in 0..self.data.len() {
            out.data[k] = self.data[k] - other.data[k];
        }
        out
    }

    /// 逐元素缩放。
    pub fn scale(&self, s: f64) -> Mat {
        let mut out = self.clone();
        for v in out.data.iter_mut() {
            *v *= s;
        }
        out
    }

    /// 矩阵乘法，O(n·m·p)。
    pub fn mul(&self, other: &Mat) -> Mat {
        assert_eq!(self.m, other.n);
        let mut out = Self::zeros(self.n, other.m);
        for i in 0..self.n {
            for k in 0..self.m {
                let a = self.get(i, k);
                if a == 0.0 {
                    continue;
                }
                for j in 0..other.m {
                    out.data[i * out.m + j] += a * other.get(k, j);
                }
            }
        }
        out
    }

    /// 解 A·X = B，返回 X。
    ///
    /// 高斯消元加部分主元。A 必须可逆。
    pub fn solve(a: &Mat, b: &Mat) -> Mat {
        assert_eq!(a.n, a.m);
        assert_eq!(a.n, b.n);
        let n = a.n;
        let m = b.m;

        // 增广矩阵 [A | B]
        let mut aug = Mat::zeros(n, n + m);
        for i in 0..n {
            for j in 0..n {
                aug.set(i, j, a.get(i, j));
            }
            for j in 0..m {
                aug.set(i, n + j, b.get(i, j));
            }
        }

        for col in 0..n {
            // 部分主元：选绝对值最大的行
            let mut pivot = col;
            let mut best = aug.get(col, col).abs();
            for r in (col + 1)..n {
                let v = aug.get(r, col).abs();
                if v > best {
                    best = v;
                    pivot = r;
                }
            }
            assert!(best > 1e-14, "矩阵奇异，第 {} 列主元过小", col);

            if pivot != col {
                for j in 0..(n + m) {
                    let tmp = aug.get(col, j);
                    let pv = aug.get(pivot, j);
                    aug.set(col, j, pv);
                    aug.set(pivot, j, tmp);
                }
            }

            // 归一化主元行
            let piv = aug.get(col, col);
            for j in col..(n + m) {
                aug.set(col, j, aug.get(col, j) / piv);
            }

            // 消去其他行
            for r in 0..n {
                if r == col {
                    continue;
                }
                let factor = aug.get(r, col);
                if factor == 0.0 {
                    continue;
                }
                for j in col..(n + m) {
                    let v = aug.get(r, j) - factor * aug.get(col, j);
                    aug.set(r, j, v);
                }
            }
        }

        // 取出右半部分
        let mut x = Mat::zeros(n, m);
        for i in 0..n {
            for j in 0..m {
                x.set(i, j, aug.get(i, n + j));
            }
        }
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solve_identity() {
        let a = Mat::identity(3);
        let mut b = Mat::zeros(3, 1);
        b.set(0, 0, 1.0);
        b.set(1, 0, 2.0);
        b.set(2, 0, 3.0);
        let x = Mat::solve(&a, &b);
        assert!((x.get(0, 0) - 1.0).abs() < 1e-12);
        assert!((x.get(1, 0) - 2.0).abs() < 1e-12);
        assert!((x.get(2, 0) - 3.0).abs() < 1e-12);
    }

    #[test]
    fn solve_small_system() {
        // 2x + y = 5
        // x + 3y = 10
        let mut a = Mat::zeros(2, 2);
        a.set(0, 0, 2.0);
        a.set(0, 1, 1.0);
        a.set(1, 0, 1.0);
        a.set(1, 1, 3.0);
        let mut b = Mat::zeros(2, 1);
        b.set(0, 0, 5.0);
        b.set(1, 0, 10.0);
        let x = Mat::solve(&a, &b);
        // x = 1, y = 3
        assert!((x.get(0, 0) - 1.0).abs() < 1e-12);
        assert!((x.get(1, 0) - 3.0).abs() < 1e-12);
    }
}
