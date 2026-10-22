# K6/R2 第三次独立复审交接 — 2026-10-22

结论口径：当前状态仍是 **READY FOR REVIEW / NOT INTEGRATED**。本文件是交给独立复审者的交接说明，不是复审结论，也不构成 APPROVE、Reviewed 或 Closed。

## 1. 为什么需要第三次复审

- 第一次复审（被审 `d3818f5`）返回 CHANGES REQUIRED，四项 R2-01…R2-04，记录见 [2026-09-28-K6-R2-rereview.md](2026-09-28-K6-R2-rereview.md)。
- 修复（`a7965ad`…`93d01c1`）后，第二次复审（被审 `2be7096`）认定 R2-03 只关闭了一半：`dispose_after_driver` 仍是 `async fn`，future 在首次 poll 之前被 drop 时，exit 与 registry 权威已被取走，而账本仍声称可做全权清理。
- `0ec8096` 之后的三个提交（`fdc8e5e`、`eeb44b5`、`a108bee`）改变了清理步骤结构、CI 门禁语义与标识符分配方式，**均未被任何复审覆盖**。第二次复审本身也没有独立文件，只以散文形式记录在 `docs/runtime/STAGE2-MILESTONE-PLAN.md` 的 2026-09-28 段落里。

因此本次复审范围是 `d3818f5..HEAD`，重点是第二次复审之后的增量。

## 2. 基线与分支

- 被审基线（第一次复审的 HEAD）：`d3818f5d2a77030f353e1a2a6c1744d494cae0c4`。
- 被审分支：`codex/k6-r2-repair`，HEAD 为本文件所在提交。相对基线的完整序列以 `git log --oneline d3818f5..HEAD` 为准：13 个修复提交，加上 CI 修复、弃用修复、等价性回归与文档对账。
- 与 `main` 的关系：`main` 仍是 `d3818f5`，是其祖先，合并可取 fast-forward；本仓库对里程碑集成使用 `--no-ff`。
- 工作树：`.workbuddy/` 已被忽略；`docs/reviews/` 已入库（README 与 Stage 2 计划都引用它）。

## 3. 本次必须逐条回应的五个焦点

1. **同步 acceptance（`0ec8096`）。** `dispose_after_driver` 由 `async fn` 改为 `fn -> CompositionReleaseFuture`，owner 校验与账本标记在调用时完成，`SweepGuard` 与 `Arc<AtomicBool>` 已删除。请核对：接受之后、future 首次 poll 之前被 drop 的路径是否真的只剩 best-effort 且不可丢失资源；`OrderlySweepInterrupted` 与对应的 `AlreadyReleased` 分支被文档称为"防御性不可达"的论证是否成立（一个 driver 只产出一个 exit）。
2. **可 park 的注册清理（`fdc8e5e`）。** `CapabilityRegistry::remove` 拆为同步 `take` + 可 park 的 `dispose_taken`，`remove` 作为二者组合保持原语义。实现方结论是：该步骤在当前组合接线中**不可达**——插件 fiber 由激活模块实例化并记入模块 fiber 列表（`plan.rs`），账本先释放这些 fiber 再处理注册（`assembly.rs::run_sweep`），而 `CapabilityFiber::dispose` 对已 `Disposed` 的 fiber 立即返回（`semantic.rs`），因此 `dispose_fibers` 无等待点。请复核该论证；若你认为可达，请给出 repro（本次未提供，改为防御性定性，ADR 0009 §3 已按此改写）。
3. **身份 v2 的 tag 拼写分歧（`63b443d`）。** v2 的非幂等 tag 用下划线，冻结的 v1 字节用连字符；两套金样各自钉住。请确认这只是文档澄清、没有放宽任何等价性判断，并确认 v1 canonicalizer 仍只在 `#[cfg(test)]` 内。
4. **CI 门禁真实性（`eeb44b5`、`a108bee`）。** 原 pin 指向 Rust 1.120.0 的 action 提交（无 `toolchain` 输入、版本写死），三个 job 全部 5 秒内失败；`msrv` job 还因为 `rust-toolchain.toml` 选择 `stable` 而一直没测 1.85。现在按工具链各 pin 一个提交、用 `RUSTUP_TOOLCHAIN=1.85.1` 强制 MSRV，并加入 1.85 clippy。请核对：门禁是否真的在观测它声称观测的东西；`fetch_update` 到 CAS 循环的替换是否保持单调性、溢出语义与 relaxed 顺序（`try_update` 在 1.85 不存在，这是不能直接采用新 API 的原因）。
5. **文档真实性。** 计划、README、ROADMAP、ADR 0009、契约与 inventory 是否与代码一致；2026-09-28 段落里"分支未 push""没有远端 CI 观测"等陈述已被 2026-10-22 段落取代，请确认没有其他残留的失实断言。

## 4. 证据与门禁

- CI：run `39719852556`（head `a108bee`）三个 job 全绿；`msrv` job 日志可见 `cargo 1.85.1`。此前两次失败 run 为 `36699645785`（toolchain pin）与 `39719615044`（`fetch_update` 弃用）。
- 本地（`a108bee` 加上 `c2870a8` 的等价性回归与后续文档变更）：`cargo fmt --all -- --check`；`cargo clippy --workspace --all-targets --all-features -- -D warnings`；`cargo test --workspace --all-features` = 346 条 test-result 行通过 / 0 failed（39 个测试二进制 + 8 个 doc-test 目标 + 5 条子进程重跑；独立父级用例 341）；`cargo +1.85 test --workspace --all-features` 同；`cargo +1.85 clippy ... -D warnings`；`cargo doc --workspace --no-deps`；`cargo run -p graph-lab`；`cargo run -p runtime-loader --example r2_host`；`fixtures/r2-host-fixture` 的 check / clippy / MSRV check。
- 第一次复审的反例与日志在 [evidence/r2-2026-09-28/](evidence/r2-2026-09-28/)；本次新增的等价性回归是 `crates/capability-graph/tests/semantic.rs::take_plus_dispose_taken_matches_one_shot_removal`。
- 实现方的自测绿灯不作为复审证据；上面的数字只是"这些命令跑过了"的记录。

## 5. 判定与交付

- **PASS**：0 blocker，且上列五项逐条给出结论（含"防御性论证成立/不成立"的明确判断）。
- **CHANGES REQUIRED**：请给出可复现的反例或明确的契约冲突，并说明是最小修复还是设计层返工。
- 复审记录请直接落在 `docs/reviews/` 并入库（本次起复审工件入库，README 与计划都引用该目录）。
- 判定为 PASS 之后才进入：`--no-ff` 合并到 `main` → 推送 → 观测 `main` CI → 写集成证据 → K6 转 Reviewed、R2 转 Closed。

## 6. 不覆盖

- 不重审 K1–K5 的全部历史代码（沿用第一次复审的范围说明：K6 差异 + 相关契约路径）。
- 不评价 Stage 3 设计；Stage 3 入口门在 R2 Closed 之后才打开。
- 不把"实现方已修"当作关闭项：任何 finding 的关闭都需要本次复审自己的证据。
