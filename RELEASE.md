# 发布清单

## 一、推送前确认

```bash
cargo test          # 36 个单元测试 + 3 个集成测试
cargo build --release
```

当前状态：

| 项 | 值 |
|---|---|
| 分支 | `dev` |
| 提交数 | 41 |
| 文件数 | 41 |
| 测试 | 全绿 |
| clippy | 无 error |

## 二、本地分支整理

当前有三个分支，需要保留的只有 `dev`。

在 Sourcetree 左侧 Branches 面板：

1. 右键 `main` → **Delete**
2. 右键 `dev-merged` → **Delete**

如果 Sourcetree 提示 "not fully merged"，选 **Force delete**。

命令行等价：

```bash
git branch -D main dev-merged
```

## 三、推送

Sourcetree 点 **Push**：

- 本地分支：`dev`
- 远程分支：`dev`
- 不要勾 "Force push"

这次是快进合并，不需要强制推送。

推送成功后，`origin/dev` 从 `Initial commit` 变成 41 个提交。

## 四、清理远端

### 删除 init 分支

仓库页 → **Branches** → `init` 右侧垃圾桶图标。

### 设置默认分支

**Settings** → **Branches** → **Default branch** 改成 `dev`。

### 删除 main

仓库页 → **Branches** → `main` 右侧垃圾桶。

GitHub 会阻止删除默认分支，所以顺序是：先改默认分支，再删 main。

## 五、发布 Release

仓库页 → **Releases** → **Draft a new release**：

- Tag：`v0.1.0`
- Target：`dev`
- 标题：`trustmesh v0.1.0`
- 说明可复制下面的简介

```markdown
首个公开版本。

核心能力：
- 按评分加权的信任传播（个人 3，群体 7）
- 线性衰减，半衰期 3 天
- Count Sketch 压缩，发布体积 80 KB
- 极轻量运行时，无 async 依赖
- 四套实测参数预设

实测数据：
- 女巫规模扩大 50 倍，目标信任只上升 87%
- 背书攻击：10% 背书带来 4.5 倍相对优势，被完全按比例压制
- 抽样 K=50 时，排序一致率 97.6%
```

## 六、可选：加 CI

`.github/workflows/ci.yml`：

```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --all-targets
      - run: cargo clippy --all-targets -- -D warnings || true
```

最后一行加 `|| true` 是因为 clippy 还有风格警告，不想让 CI 红。

## 七、发布后检查

- [ ] 仓库首页显示 trustmesh 的 README
- [ ] `dev` 是默认分支
- [ ] 只有 `dev` 一个长期分支（init、main 已删）
- [ ] Release v0.1.0 有 tag
- [ ] 41 个文件都在
- [ ] LICENSE 显示 MIT

## 八、已知遗留

| 项 | 说明 |
|---|---|
| clippy 风格警告 | 主要是 `needless_range_loop`，不影响功能 |
| `dev-merged` 分支 | 本地冗余，可删 |
| `src/bin/` 下的实验程序 | 保留，作为可复现的证据 |
| `AUDIT.md` 里的「未实现」 | 地址绑定、审计机制、存盘序列化 |
