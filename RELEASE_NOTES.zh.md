# 本发行来源分支：`feat/airgap-bulk-push`

这是 **coloraven** 对 [artifact-keeper/artifact-keeper-cli](https://github.com/artifact-keeper/artifact-keeper-cli) 的**分叉发行**，不是上游官方 Release。

- **来源分支（显著）：`feat/airgap-bulk-push`**
- **对照基准：** 上游 `v1.2.0`（`chore(release): v1.2.0`）之后的分叉提交
- 配套服务端分支：`artifact-keeper` 的 `feat/ferry-ingest`

---

## 相对 fork 基准新增的功能

### 空气隔离下载与入库

- `ak download --go|--npm|--pypi|--cargo`：用各语言工具链在外网拉包，打成 ferry 包。
- 默认 zip 文件名自动生成；可用 `--no-archive` 写成**目录树**（TB 级避免单 zip）。
- `ak artifact push <repo> --from-dir` / `--from-archive`：把 ferry 送进内网仓库。
- `ak repo catalog` 导出 JSONL，下载时 `--catalog` 跳过内网已有包。
- Cargo ferry 使用隔离 workspace，不污染用户全局 cargo 目录。

### 断点续传

- 下载 SQLite 缓存：半成品 / sha 不匹配则作废重拉。
- Cargo HTTP：`.partial` + `Range` 续下，成功后才写入 cache（官方 CDN 回退 `static.crates.io`）。
- `--no-archive` 第二次运行会校验已有输出目录，完好的包直接 skip（`Resuming download: N package(s) already intact`）。
- Go：`.zip` 与 `.mod` **都在**才 skip；缺一则补传。
- `ak artifact push --skip-dupe-uploads`：服务端 409 / immutable 计为 skipped，并打印 `Resuming: skipped N already on server…`。
- 大文件分片 finalize 失败时**保留本地 session cache**，再次 push 可续传同一 session。

### 构建

- `feat/**` / `fix/**` push 由 GitHub Actions 出 Windows / Linux amd64 CLI；禁止本机 `cargo` 编译。
