# rustup I/O → async：分析、PoC 与性能门禁

分析基线：`615e345a3fb1e067b43f043029f3bdb0b57bc189`，rustup 1.30.0；本地验证日期 2026-09-19。

建议先迁移**调度和完成通知**，保持文件系统操作的粒度、并发度与缓冲复用。把每个 `std::fs` 调用分别改成 `tokio::fs` 并不能保证性能持平。Tokio 1.53.1 的普通文件 I/O 仍通过阻塞线程池执行，官方也建议把多个操作合并到尽量少的 `spawn_blocking` 调用中。[Tokio fs 文档](https://docs.rs/tokio/1.53.1/tokio/fs/index.html)

本次 PoC 提供 `unpack(...).await`，复用真实的 rustup tar 解包器，给现有磁盘层增加可选的 Tokio 后端；生产 CLI 仍选择原后端。它验证第一阶段替换的可行性，不代表已把下载、tar 解析、安装事务全部改成 async，也不是 io_uring 实现。

对 issue 中“async 能否替代 `RUSTUP_IO_THREADS=1`”的回答是：async 可以统一调度和生命周期、改善等待方式，但不会自动消除 OOM 或系统调用延迟。并发限制与低内存回退仍然必要；尤其解压窗口的内存不能靠减少 async task 数解决。

## 当前代码实际做了什么

| 路径 | 当前行为 | 迁移判断 |
|---|---|---|
| `src/bin/rustup-init.rs::main` | Tokio runtime；async worker 数来自 `io_thread_count()` | async worker 数与磁盘在途操作数应分开配置；`worker_threads` 不限制 blocking pool |
| `src/download/mod.rs::Download::{download_impl,execute}` | reqwest 请求和 body stream 已 async；`OpenOptions`、断点续传读取、`write_all`、本地 `file:` 读取、`sync_data` 仍同步 | 优先移出 runtime worker；按下载流合并写入、校验、落盘，保留错误/断点续传语义 |
| `src/dist/download.rs::{download,file_hash}` | cache 命中校验、读取/删除/重命名仍同步；哈希读取块 32 KiB | 整个“读取+SHA256”作为阻塞工作，而非每 32 KiB 调度一次；不要把 CPU 哈希直接塞进 async poll |
| `src/dist/manifestation.rs::InstallEvents` | 默认 2 路下载；单个安装事务通过 `spawn_blocking` 执行；下载和安装可以重叠 | 不要声称当前安装仍全部堵塞 runtime；保持事务串行，避免同时更新同一 prefix |
| `Manifestation::update_v1` | legacy v1 的解包和安装仍在 async 函数里同步执行 | 单独迁移完整操作边界，不能遗漏 v1 |
| `src/dist/component/package.rs` | 同步 tar + gzip/xz/zstd；验证路径/类型、规范化 mode；`DirStatus` 管理父目录依赖；超过 16 MiB 的文件分块送给 writer | decoder 与 tar iterator 留在专用生产线程；async 化依赖和完成事件，不要串行等待每次 mkdir |
| `src/diskio/{mod,threaded,immediate}.rs` | 同步 `Executor` trait；独立 `threadpool`；完整文件在一个 worker 内 open/write/close；增量文件持有打开的文件并接收后续块 | 必须保留 close 的并发、单文件顺序、错误完成通知和 drain；本 PoC 的主要替换边界 |
| `src/dist/component/transaction.rs`、`src/utils/{mod,raw}.rs` | move/copy/remove/rename、锁文件和 Drop 回滚同步；还存在跨设备复制与平台重试语义 | 按事务/目录树等完整单元隔离阻塞工作；不能仅改签名，也不能让取消绕过 rollback |
| CLI stdin/stdout、代理进程、设置读取 | 大量短同步 I/O；有些是启动或交互路径 | 按实测收益迁移；不能给每次代理 rustc 增加新的文件任务、缓冲和启动开销 |

当前 `Threaded` 的约束是实际迁移合同：

- 默认磁盘线程数为可用 CPU 数，上限 8；`RUSTUP_IO_THREADS=1` 选择 Immediate。自动配置且解包预算低于 512 MiB 时也回退 Immediate；显式线程数覆盖回退。
- 五档池：4 KiB、8 KiB、1 MiB、8 MiB、16 MiB；初始预留约 25 MiB。`RUSTUP_UNPACK_RAM` 至少 32 MiB。预算约束的是池的高水位，**不是进程 RSS**，目录图、slab 元数据、线程栈、解压窗口等另占内存。
- 提交端在 queued count 达到 5 时等待完成事件；执行并发由线程数决定。目录未完成时子项挂在 `Pending`，不能提前写入，也不能重复创建每个父目录。
- 大文件每块 16 MiB；块写完、释放引用后 ACK 才能归还预算。空块表示 EOF，最终 Item 完成必须晚于 close。
- `completed()` 使用非阻塞轮询；包解压在预算不足时会忙等。原 `join()` 还每 100 ms 检查一次计数，再 join 线程池。两者都是可单独优化的点，不能把消除 100 ms 尾延迟解释为“async 文件 syscall 更快”。
- tar 路径拒绝绝对路径和 `..`，拒绝 link/device 等类型，移除包的首层目录并规范化权限。PoC 直接共用这些检查。当前解包写文件使用 `std::fs::OpenOptions`；issue 评论中提到的 `at` syscall 不能直接作为这一版本解包代码的事实，当前 `fs_at` 主要见于 toolchain 检查。

还有一个资源生命周期问题：`ComponentBinary::download` 在把组件加入安装队列之前就创建 executor 和 temp dir，所以多个尚未安装的组件可能同时持有线程池/缓冲池。正式迁移应在开始安装时按需创建，并使用全局额度。PoC 的两种后端均在生产线程开始解包时构造，用来公平比较后端；这不测量现有下载队列持有多个 executor 的额外资源。

原作者在 issue 中明确说明了 Windows close 延迟、NFS mkdir 依赖、低内存和大文件流式写入这几类约束。[设计背景](https://github.com/rust-lang/rustup/issues/4159#issuecomment-2602389995)

## PoC 的实现边界

```text
async 调用者：unpack(...).await
          │ oneshot 返回结果（输出 TempDir 随结果一起移交）
          ▼
专用 tar 生产线程：原 tar/decoder + DirStatus + 原缓冲池
          │ 最多 5 个待执行工作；原有字节预算限制数据
          ▼
Tokio blocking pool：每个依赖阶段最多 N 个有限生命周期的 worker
          │ 一个 worker 连续处理多个完整文件/目录操作
          │ 大文件在同一 worker 上接收和写入顺序块，最后 close
          ▼
原完成通道：Item / Chunk → 释放预算、唤醒依赖项
          │
async supervisor await 所有 worker → 发出阶段完成通知
```

入口在 [`src/diskio/poc.rs`](../src/diskio/poc.rs)，调度器在 [`src/diskio/tokio_pool.rs`](../src/diskio/tokio_pool.rs)。`Threaded::new_tokio` 替换 worker 来源，其余池、ACK、背压与解包代码共用。因此这个 PoC 的“async”在调用接口、runtime 资源管理和 supervisor 等待层；tar 生产端与 `Executor` trait 暂时保持同步。后续完整 async API 的迁移见下文。

第一版曾每个 Item 调用一次 `spawn_blocking` 并用 `JoinSet` 调度；实测 docs 的 CPU 用量约增加 35%，因此弃用。当前版本按一个有结束条件的解包阶段运行 N 个 worker，一次调度处理多个文件，保持逐文件背压。阶段 join 关闭提交队列，等待所有 worker 返回；若随后目录完成又释放子项，再开启下一阶段。没有永久后台 worker。

生产线程不占 Tokio blocking pool 的槽位，避免“外层解压占一个 blocking slot、等待同一 pool 的 writer”在 `max_blocking_threads(1)` 下死锁。即使只剩一个可用阻塞线程，排队的 worker 也能在队列关闭后依次结束。runtime 必须在操作 drain 期间保持运行。[spawn_blocking 的线程与取消语义](https://docs.rs/tokio/1.53.1/tokio/task/fn.spawn_blocking.html)

取消入口 future 不会立即中断 syscall。生产线程持有输出目录直到任务全部 join；调用者已离开时，发送失败会销毁已完成的 TempDir，避免删除仍在写入的目录。这是安全清理语义，不是快速取消。接入正式安装事务前，还需 stop-admission、关闭增量 sender、drain、rollback 的显式取消协议。runtime 本身提前关闭不在此 PoC 的支持范围。

PoC 会占用最多 N 个 blocking worker 直到该阶段结束；正式集成时要为下载、DNS 和其他阻塞工作留出容量，使用全局磁盘并发额度，避免多个 component 各自开 N 个 worker。不要把 `max_blocking_threads` 当成唯一的磁盘并发控制。

## 在真实路径上发现并修复的共用问题

这些修复同时作用于基准的 Threaded 和 Tokio 后端，避免把修复收益误算为后端收益：

1. 增量文件原本 drop slab 引用却没有 `clear`。预算计数虽然归还，实际 slot 没有回收；512 MiB 文件在 64 MiB 预算下曾测到约 516 MiB RSS。仅在 worker 中 clear/drop 仍会进入远端 free list，而 sharded-slab 优先消耗尚未使用的本地 slot，可能继续扩容。现在在写入前标记 clear，把 buffer 随 ACK 送回分配线程，释放最后引用后再归还预算。回归测试检查实际 buffer 地址复用，避免只验证账面计数。
2. tar 读取失败使增量 sender 提前关闭，原 `recv().unwrap()` panic 后不会发送 Item 完成，也不减少 `n_files`，join 可能永久等待。现在转换成 `UnexpectedEof`；worker panic 也转换为 Item 错误并归还完成计数。
3. 隐式父目录之后再出现显式同名目录条目，原代码会覆盖 `Pending` 或重复处理完成通知。现在保留已经存在或正在创建的目录状态。

## 如何继续迁移

**第一阶段：本 PoC。** 独立验证 worker 来源和完成等待；保留 feature gate 和原后端。先提交共用 bug 修复，再独立评估后端改变。新后端不应在性能矩阵未通过时直接替换默认实现。

**第二阶段：让调度协议可 await。** 将 `execute()/completed()/join()` 的 Iterator 协议替换成有界命令通道、async 完成流和 `finish().await`。生产线程只负责 decoder/tar 读取和输出已校验的条目；async 协调器维护原 `DirStatus`，同时处理父目录完成和后续条目。字节预算在分配前获取，按 bucket 的 capacity 计费，直到写完且最后引用释放后才归还。metadata/目录控制消息需能在数据预算耗尽时推进，避免父目录任务被子文件占满预算后饿死。还要限制 pending 条目数量和路径元数据；单独的 `mpsc(5)` 并不限制整个目录图。

此阶段消除 `flush_ios` 的忙等；不能在每个文件 `submit().await` 后立即 `join().await`，那会丢掉并发。大文件必须边解压边写，不能先把整份 LLVM 文件读进 Vec，也不要为了纯 async API 改成每 32 KiB 一次跨线程往返。先保持 16 MiB 块与小文件池，任何 buffer/并发调参单独 benchmark。

若完成事件改由 async 协调器处理，仍需把 slab buffer 送回分配线程释放，或换成不依赖线程归属的有界对象池。直接在可跨 worker 迁移的 async task 中 drop，会重新引入本次发现的远端 free list 扩容问题。

**第三阶段：下载、缓存与事务边界。** 下载 body 通过按字节限额的通道交给一个有结束条件的阻塞 writer，保持网络 backpressure；writer 顺序处理 resume 读取、hash、write 和最后的 `sync_data`，将最终结果交回 async 调用者。必须保留 Range/206/416、部分文件保留策略、hash 校验后 rename 的顺序。完整文件 cache 校验单次 offload。v1 安装也移出 runtime worker。事务仍按原顺序提交/回滚，析构清理不与后台写入竞争。

如果采用 `tokio::fs::File`，需要考虑其内部缓冲/复制和 write 的完成语义；必须在确认完成、rename 或清理前等待 `flush()`，有原 `sync_data` 要求的路径还要保持持久化步骤。解包原本不调用 fsync，基准也不能额外加 fsync 后拿来比较。[Tokio 文件写入与 flush](https://docs.rs/tokio/1.53.1/tokio/fs/index.html)

**第四阶段：可选原生异步后端。** Linux io_uring、Windows 对应 API 只有在前三阶段的合同稳定、实测 syscall/调度仍为瓶颈时再评估。跨平台、close 延迟、descriptor 生命周期、buffer pinning、取消和 fallback 成本必须一并计算，不能用单个平台的吞吐替代 rustup 的整体结论。

## 性能持平如何成为可验证要求

建议验收阈值：同机器、同构建配置、同 workload、同线程数和 RAM 预算，配对耗时比的一侧 95% bootstrap 上界 ≤ 1.05，CPU 时间对应上界 ≤ 1.10，峰值 RSS 中位配对比 ≤ 1.10。阈值是本次明确提出的门禁，不是 issue 已规定的数字。还要确认内容/权限一致、无挂起、文件句柄和线程数受限，以及 async reactor 不被同步 I/O 阻塞。

[`scripts/bench-async-io.py`](../scripts/bench-async-io.py) 会生成 2 万小文件、512 MiB 流式文件和覆盖各 bucket 的混合 tar。每种组合预热一次，再交替 A/B 顺序，在独立进程中重复测量；每次输出目录全新，输入 page cache 为热。提取计时包含线程/池初始化与 drain，不包含后续内容校验、清理、网络和安装事务。校验对全部相对路径、类型、mode、长度和文件内容做 SHA256，所有运行必须一致。生成器和测试中的已知内容提供额外正确性验证。

CPU 和 RSS 在解包结束、校验之前采样；CPU 是 `getrusage` 的提取区间增量。Linux RSS 使用 `/proc/self/status` 的 `VmHWM`，避免 `getrusage.ru_maxrss` 跨 exec 保留启动器的峰值、掩盖小工作负载的内存差异；其他 Unix 使用 `ru_maxrss`，正式测量还应排除启动器影响。脚本保留全部样本和汇总；门禁失败时返回非零。Windows 示例不提供这些指标，脚本会拒绝据此声称通过，需接入平台 profiler。[getrusage 的 exec 语义](https://man7.org/linux/man-pages/man2/getrusage.2.html)

```bash
cargo test --locked --lib --features test,async-io-poc
cargo clippy --locked --all --all-targets --all-features -- -D warnings
cargo build --locked --release --no-default-features \
  --features reqwest-rustls-tls,async-io-poc --example async-io-poc
python3 scripts/bench-async-io.py \
  --work-dir /path/on/test/filesystem/async-io-bench \
  --threads 1 4 8 --ram-mib 32 64 --rounds 9
```

脚本自动从 `cargo metadata` 找到 target 目录。也可传 `--binary`，或用 `--archive /path/to/rust-docs.tar.xz` 测真实组件，后缀支持 `.tar`、`.gz`、`.xz`、`.zst`。示例本身提供 `--automatic-threads` 检查低内存回退，`--blocking-threads 1` 检查阻塞池受限行为。

也可以单次运行；程序提取到指定父目录下的私有临时目录，校验后清理，并输出一行 JSON：

```bash
cargo run --release --features async-io-poc --example async-io-poc -- \
  --backend tokio --archive /path/to/component.tar.xz \
  --output-parent /path/on/test/filesystem --threads 4 --ram-mib 64
```

正式切换还需要：Linux 本地盘、Windows Defender 开启、macOS、真实 NFS/高延迟存储、低内存容器/ARM，以及真实 rust-docs/rustc/rust-std 的 gzip/xz/zstd 包；加入冷缓存、完整 install/update、同时下载与安装和取消/ENOSPC 场景。报告单项失败，不能以平均加速掩盖退步。高延迟 mock 可用于回归并发特性，不能替代真实 Windows/NFS 测量。

本地实测结果和检查记录见下方；平台未测部分不能视为通过。只有通过目标部署矩阵后才能启用新后端作为默认值。

## 本机验证结果

**22/22 组通过本报告的耗时、CPU、RSS 门禁，全部输出 fingerprint 一致。** 共 396 次正式运行，另有 44 次预热。合成负载完整覆盖 1/4/8 并发 × 32/64 MiB；官方 rust-docs 覆盖 1/4/8 并发、64 MiB；官方 rustc 补测 4 并发、64 MiB。两份官方包均先校验发行站的 SHA256。

下表展示 4 并发、64 MiB 预算的各项中位耗时、配对比的 95% 上界和 RSS 中位数。比值为 Tokio / Threaded；Threaded 基准也包含共用正确性修复。

| 负载 | Threaded 耗时 | Tokio 耗时 | 耗时比上界 | CPU 比上界 | RSS：Threaded / Tokio |
|---|---:|---:|---:|---:|---:|
| 合成 docs：2 万小文件 | 0.411 s | 0.304 s | 0.785 | 1.025 | 5.2 / 5.1 MiB |
| 合成 large：512 MiB + 17 B | 0.253 s | 0.158 s | 0.624 | 1.009 | 52.0 / 52.1 MiB |
| 合成 mixed：各档 buffer + 流式文件 | 0.209 s | 0.108 s | 0.532 | 1.007 | 28.6 / 28.6 MiB |
| 官方 rust-docs 1.90.0（XZ） | 2.547 s | 2.459 s | 1.011 | 1.016 | 106.1 / 102.1 MiB |
| 官方 rustc 1.90.0（XZ） | 2.787 s | 2.679 s | 0.964 | 1.000 | 108.4 / 108.8 MiB |

机器：Ryzen 7 5800H、Linux x86_64、NVMe/Btrfs（compress=zstd:1），Rust 1.98.1。具体环境、构建选项、采样边界与测试记录见 [验证记录](async-io-results/README.md)。

合成负载有明显收益，其中一部分来自去掉原 join 的 100 ms 轮询尾延迟；真实压缩包仍有解压等成本，收益更温和。不能将合成测试的加速百分比当成完整 rustup 安装的加速比例。未压缩 512 MiB 文件在 32 MiB 预算下，两后端峰值 RSS 均约 20 MiB；实际 XZ rust-docs 的进程峰值更高，说明解压窗口仍需单独计入总内存设计。

本地证据支持这一阶段在所测环境达到 performance parity。**Windows、macOS、NFS、低内存容器/ARM、冷缓存与完整 install/update 性能尚未验证，不能宣称跨平台无条件持平。** 因此新后端保持显式 PoC feature/入口，生产默认切换必须执行前述部署矩阵与门禁。

可检查的原始证据：

- [合成汇总](async-io-results/synthetic.json) / [360 条样本](async-io-results/synthetic-samples.jsonl)
- [官方 rust-docs 汇总](async-io-results/rust-docs.json) / [60 条样本](async-io-results/rust-docs-samples.jsonl)
- [官方 rustc 汇总](async-io-results/rustc.json) / [20 条样本](async-io-results/rustc-samples.jsonl)
- [源码摘要](async-io-results/source-sha256.json) / [依赖锁文件](async-io-results/Cargo.lock)
