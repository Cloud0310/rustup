# 验证记录

代码基线：`615e345a3fb1e067b43f043029f3bdb0b57bc189` 加当前 PoC 改动。源码摘要在 `source-sha256.json`，二进制摘要和输入归档摘要在各 report 的 `environment` / `inputs` 中。两种后端都包含共用的正确性修复；不是把修复前后的差异计入 Tokio 收益。

本机为 AMD Ryzen 7 5800H（8 核 / 16 线程），Linux x86_64，Rust 1.98.1。测试文件系统是 NVMe 上的 Btrfs，挂载选项包含 `compress=zstd:1`、`autodefrag`、`commit=120`。使用默认 release 优化（LTO、1 codegen unit），`--no-default-features --features reqwest-rustls-tls,async-io-poc` 构建示例。runtime 为 current-thread，blocking pool 上限 32，栈大小 1 MiB；磁盘并发独立取 1/4/8。

输入 page cache 为热，每次输出目录全新。采样区间包含 decoder 初始化、解包、pool 初始化和 drain；不包含校验与清理。CPU 是进程 user+system 时间差，Linux RSS 是本程序地址空间的 VmHWM。未调用额外 fsync，因此这些数据不是物理盘持久化吞吐，不能推广到其他文件系统或 Windows Defender/NFS。

每组合有 1 对预热和 9 对正式样本；A/B 顺序交替。`paired_ratio` 为 Tokio / Threaded，小于 1 表示更少。`upper95` 是对配对比中位数 bootstrap 得到的一侧 95% 上界；p95 在 9 轮下只是描述性统计。门禁分别为耗时上界 ≤ 1.05、CPU 上界 ≤ 1.10、RSS 配对比中位数 ≤ 1.10，且每次输出完整 fingerprint 必须一致。样本文件保留了预热行（repetition=-1），汇总排除预热。

已执行的正确性检查：

| 命令 | 结果 |
|---|---|
| `cargo test --locked --lib --features test,async-io-poc` | 136 passed，4 ignored |
| `cargo test --locked --features test,async-io-poc --test test_bonanza suite::cli_v2::install_` | 13 passed |
| `cargo test --locked --lib --features test diskio` | 6 passed，验证未启用 PoC 的路径 |
| `cargo test --locked --lib --features test,async-io-poc diskio` | 13 passed；强化大文件块顺序检查后重新通过 |
| `cargo clippy --locked --all --all-targets --all-features -- -D warnings` | 通过，包括最终示例和强化后的测试 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |

PoC 测试覆盖四种输入（tar/gzip/xz/zstd）、完整内容/块顺序、空文件、Unix mode、隐式父目录与重复目录条目、恶意路径与符号链接拒绝、截断归档、文件系统错误、取消后的 drain/清理、单个 blocking slot、并发上限、reactor heartbeat 和实际 slab buffer 复用。

另用 release 示例在完整 512 MiB 文件上验证 `--backend tokio --threads 4 --blocking-threads 1` 和 `--backend tokio --threads 8 --automatic-threads --ram-mib 32`；二者均成功完成，输出与 `--backend threaded --threads 1` 的 fingerprint 一致，覆盖公开入口的阻塞池受限和低内存自动回退。

一次额外的 `--no-default-features --features reqwest-rustls-tls,test,async-io-poc` 完整库测试有 1 个既有 SOCKS 测试失败：测试直接构造 reqwest client 时未安装 rustls crypto provider。没有修改该下载测试或 TLS 配置；上表使用项目默认 TLS features 的完整测试已通过。PoC 的 release 示例可用 Rustls-only 配置成功构建和测量。

`Cargo.lock` 保存了本次依赖集合。若要精确复现，请在独立 checkout 中用该文件作为根 Cargo.lock，并使用同一编译器；不同硬件、文件系统、缓存和负载下需要重新执行门禁。

尚未验证：Windows/macOS、真实 NFS/Defender、冷缓存、低内存容器/ARM、完整安装更新的性能、ENOSPC 和 runtime 提前关闭。本 PoC 不启用为生产默认后端。
