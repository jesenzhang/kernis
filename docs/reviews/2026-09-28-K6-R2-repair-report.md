# K6/R2 修复执行报告 — 2026-09-28

结论：**READY FOR REVIEW / NOT INTEGRATED**。R2-01…R2-04 的实现与回归已在本地全量门禁下通过；本次没有 push、没有 merge、没有独立复审，因此 R2 未 Closed，Stage 3 未启动。本报告不把实现方自测绿当作任何 APPROVE/PASS 证据。

## 1. 基线、分支与 HEAD

- 复审报告：[2026-09-28-K6-R2-rereview.md](2026-09-28-K6-R2-rereview.md)，结论 CHANGES REQUIRED / R2 NOT CLOSED（2×P1 + 2×P2），证据目录 [evidence/r2-2026-09-28/](evidence/r2-2026-09-28/)。
- 执行指令：[2026-09-28-K6-R2-repair-handoff.md](2026-09-28-K6-R2-repair-handoff.md)。
- Base（被审 HEAD，同时是分支起点）：`d3818f5d2a77030f353e1a2a6c1744d494cae0c4`。
- 分支：`codex/k6-r2-repair`（自 `main` 的上述基线）。
- 提交（按序）：
  - `a7965ad` ADR 0008 / ADR 0009 + 三个复现反例（按指令放入三个 crate 的 `tests/`）；
  - `08c0e22` R2-01 结构无歧义身份 v2；
  - `5a5ebcb` v2 数据标记从 `@` 改为 `%`（避开宿主 `module@version` 分隔符）；
  - `06c88e7` R2-02 + R2-03；
  - `29a2305` R2-04 loader-only fixture 与词汇回归；
  - `c634ce9` 契约/清单/ADR/README 状态与迁移说明；
  - `5215c86` `OwnerLossReleaseError` 文档语义与 ROADMAP 状态；
- `93d01c1` R2-03 追加“release future 在首次 poll 之前被 drop”回归 + ADR 0009 证据条目 + Stage 2 plan 的本次复审记录。
- 撰写本记录时的分支 HEAD：`93d01c1`；追加本记录的提交是分支 tip，且只改文档。未集成，因此没有 “final integrated HEAD”。
- 未跟踪内容保持原样：`.workbuddy/`、`docs/reviews/`（含复审证据）均未提交、未修改。

## 2. 精确文件范围（实际 vs 指令）

| 文件 | 本次作用 | 指令范围 |
| --- | --- | --- |
| `crates/runtime-core/src/definition.rs` | v2 编码（tag/count/长度定界）、`%` 数据标记、v1 canonicalizer 收敛到 `#[cfg(test)]` 金样 | 是 |
| `crates/runtime-core/src/lib.rs` | v2/legacy 格式常量、`LegacyDefinitionIdentityUnsupported`、`DriverOwner` 等 re-export、`StoreRevision` 透出 | 是 |
| `crates/runtime-core/src/async_driver.rs` | `DriverOwner`、`DriverExit::owner`、`RuntimeHandle::owner`；mailbox 仍是唯一 owner 生命周期事实源 | 是 |
| `crates/runtime-composition/src/assembly.rs` | owner 绑定校验、`CleanupLedger`/`SweepGuard`、`CompositionReleaseOutcome`/`CompositionReleaseRejection`、ledger-first 优先序 | 是 |
| `crates/runtime-composition/src/error.rs` | 仅文档：`OwnerLossReleaseError` 变体语义（完成优先、中断例外）；变体与 Display 文案未改 | 是 |
| `crates/runtime-composition/src/lib.rs`、`crates/runtime-loader/src/lib.rs` | 宿主词汇 additive re-export | 是 |
| `crates/runtime-core/tests/{k2_declarative,k3_async}.rs` | v2 金样与 `@` 守卫；owner token 两条语义回归 | 是 |
| `crates/runtime-composition/tests/{k4_cleanup_ownership,k4_compat,k4_composition}.rs` | 仅 API 迁移（`into_released()`），断言含义不变 | 是 |
| `crates/runtime-composition/tests/review_r2_probes.rs`（新） | R2-02 反例转正式回归（3 项，含资源零触碰与拒绝后可用性） | 是（允许新增聚焦回归） |
| `crates/runtime-composition/tests/k6_r2_cleanup_interruption.rs`（新） | R2-03 取消矩阵（3 项） | 是 |
| `crates/runtime-core/tests/review_r2_identity.rs`（新） | R2-01 反例回归 | 是（指令要求放入） |
| `crates/runtime-loader/tests/review_r2_exports.rs`（新） | 宿主词汇回归（含 test 体，不会静默跳过） | 是 |
| `crates/runtime-loader/tests/{k5_composition,k6_r2_end_to_end}.rs`、`examples/{r2_host,host_end_to_end}.rs` | API 迁移；`k5_composition:304` 的 `@` 守卫原样保留且通过 | 是 |
| `fixtures/r2-host-fixture/{Cargo.toml,Cargo.lock,src/lib.rs}`（新）、`.gitignore` | loader-only 编译期宿主 fixture + 忽略其 target | 是（“仅依赖 loader 的宿主编译 fixture”） |
| `docs/architecture/0008-*.md`、`0009-*.md`（新）、`0005`、`0007` | 身份与 owner 绑定 ADR；K4/词汇交叉引用 | 是 |
| `docs/runtime/{COMPATIBILITY,R2-KERNEL-CONTRACT,K6-supported-api-inventory,K2-*,K3-*,K4-*,STAGE2-MILESTONE-PLAN}.md`、`README.md`、`docs/ROADMAP.md` | 状态、契约、迁移说明、第四波词汇闭合 | 是 |
| `crates/runtime-core/tests/{k3_owner_state,k3_review_owner_drop}.rs` | 在范围内但无需修改（现契约已覆盖） | 是（未改动） |
| `crates/workflow-recovery/`、`crates/capability-graph/` | **未改动**：身份 ADR 不需要改持久化或物理格式；fiber disposal 复用既有 `CapabilityFiber::dispose`，未新建 lifecycle authority | 是（默认不改） |

范围外新增只有 `fixtures/r2-host-fixture/`（指令要求）与 `.gitignore` 的两行忽略。

## 3. 兼容性决策（先于实施）

- **ADR 0008** `kernis-run-definition-v2`：v2 为当前写格式；v1 冻结为“历史解释”而非写入格式（canonicalizer 留在 `#[cfg(test)]` 并固定金样字节）；带 v1 tag 的 durable 定义身份在载入任何 durable 状态之前被 `RuntimeError::LegacyDefinitionIdentityUnsupported` fail-closed 拒绝；无自动迁移、无重新采用、无迁移工具承诺；`kernis-workflow-replay-v1` 算法与 `restore_run` 路径未受影响。数据标记使用 `%`，因为 `@` 是宿主可见的 `ModuleReference` 分隔符。
- **ADR 0009**：`DriverOwner` 为 K3 command mailbox 的不透明身份；`dispose_after_driver` 先证明退出属于本 handle 绑定的 driver；清理进度住在 handle 的 ledger 里，已启动步骤被再次 poll 而非重启；rejection 载荷装箱；`release_after_owner_loss` 的 ledger 完成检查优先于 driver 状态观察。§5 记录了实现与原草案的差异（类型定名、装箱、实际 sweep 机制）。
- **ADR 0007 §2 补充**：闭合规则不变，但闭合**证据**从 import 清单改为非 workspace 成员、只依赖 `runtime-loader`、逐值显式标注类型的编译期 fixture，stable 与 MSRV 都编译。

## 4. Finding → 修复 → 失败/通过证据

### R2-01 / P1 — v1 身份编码结构碰撞，错误定义可通过恢复

- 基线失败证据：`evidence/r2-2026-09-28/baseline-r2-01-identity.log` — 两个语义不同的定义产出**完全相同**的 v1 身份串，`restore under different semantics accepted=true`，反例断言 “restore must reject a changed prerequisite/capability contract” 失败。
- 修复：v2 采用字段 tag（`#`）、集合计数（`[n]`）与显式结束符、值长度定界（`%<len>:`）；`definition.rs` 金样固定规范串，并保留对抗值集合（模仿 v1 框架、v2 框架、计数、`@`/`%` 标记的值都只是数据）。
- 附带修复（`5a5ebcb`）：v2 最初用 `@` 做数据标记，撞上被审基线就存在的 loader 守卫 `k5_composition` scenario_o（durable 定义身份不得含 `@`）。由于 v2 尚未发布，改的是格式标记而不是放宽既有断言，并在 ADR 0008 记录理由；`k2_declarative` 新增同义守卫。
- 通过证据：`review_r2_identity` 1 passed；`k2_declarative` 15 passed；`k5_composition` scenario_o 未改动并通过；`+1.85` 同绿。兼容性文档：COMPATIBILITY §2/§7、K2 文档、R2 contract 构造段。

### R2-02 / P1 — 正常清理接受其他驱动的 DriverExit

- 基线失败证据：`baseline-r2-02-03-probes.log`（2 failed）；复审反例说明 A 组合在自身 driver 仍 `Running` 时被 B 的退出清理掉资源并谎报 `rollback.is_success()`。
- 修复：`RuntimeHandle::owner() -> DriverOwner`（私有 `CommandMailbox` 克隆，`Arc::as_ptr` 身份比较，`Debug` 不暴露 mailbox 细节）；`DriverExit::owner()`；`into_driver` 把 handle 绑定到同一 token；`dispose_after_driver` 在 owner 校验通过前不运行任何 hook、不 dispose fiber、不 unregister，拒绝返回 `CompositionReleaseRejection::ForeignDriverExit { exit }`（退出原样归还）或 `AlreadyReleased { exit }`；正常路径返回 `CompositionReleaseOutcome::Released`（装箱的 `CompositionDriverShutdown`）。
- 通过证据（`review_r2_probes` 3 passed，覆盖指令矩阵）：A `Running` + B 退出被拒；A `Shutdown` + B 退出被拒；同 RunId/同配置的另一实例仍被拒（身份是 mailbox 对象，不是其内部值）；owner 丢失 + 另一 driver 退出被拒；每次拒绝断言 hook 启动数、fiber dispose 数、registry removal 数**全为 0**，且被拒退出仍能干净释放它自己的组合（清理恰好一次）；`k3_async` 两条 token 语义测试；`r2_host` example 输出完整生命周期。矩阵里的“拒绝后 A/B 各自正常清理”与“正确来源只清理一次”不是另起一轮：fixture 在同一次运行内先 `assert_refused`（零副作用），再用被拒的那份退出走它自己的 `release_with_own_exit`，两者共用同一组计数器，所以“拒绝无副作用”和“正确来源恰好清理一次”是对同一份状态的两个断言。

### R2-03 / P2 — 取消 owner-loss 清理丢失剩余账本、重试误报 AlreadyReleased

- 缺陷：`cleanup_modules` 先 `pop` 再 await，账本住在 future 内部；取消后重入回答 `AlreadyReleased`。
- 修复：`CleanupLedger` 归 `CompositionHandle` 所有，持有剩余队列、当前模块、**在飞的 dispose hook / fiber disposer**、已收集失败、orderly-interrupted 标志；`await_parked` 复用已 parked 的步骤 future，`SweepGuard` 在调用方 future 被 drop 时记账；registry removal 不可 park，故记为“仍 outstanding”的中断步骤而不盲重试；真实完成后才 `ledger = None`，`release_after_owner_loss` 先查完成再查 driver 状态；顺序清理被打断且 authority 已随 future 释放时，`dispose_after_driver` 回答 `OrderlySweepInterrupted`，剩余工作只能经 `release_after_owner_loss` best-effort 完成，并把无法撤销的注册如实报成 `CleanupResource::PluginRegistration` 失败。
- 通过证据（`k6_r2_cleanup_interruption` 4 passed）：release future 在首次 poll 之前被 drop（此时 ledger 尚未弹出任何模块，重入必须完成全部清理）、dispose hook Pending 时取消、fiber disposal Pending 时取消，以及被打断的顺序清理只能 best-effort 续跑并如实上报；前三种取消点上 hook 启动次数都不超过 1；重入继续未触碰模块、保留先前失败进入最终 `RollbackReport`；只有在真实完成后第二次调用才 `AlreadyReleased`；不存在把取消谎报为 `AlreadyReleased` 或空成功的路径。指令要求“继续验证”的四项沿用既有回归并可追溯：reverse-order 与 exactly-once 见 `k4_composition::scenario_a_composes_starts_drives_and_cleans_exactly_once`；单项失败后继续见 `k4_cleanup_ownership::startup_rollback_unregisters_registrations_after_a_fiber_cleanup_failure` 与中断文件第二个用例（失败进入最终报告）；registry authority 丢失如实上报见 `owner_loss_release_disposes_locally_once_and_reports_lost_authority`；正常 shutdown 不降级为 owner-loss 见 `orderly_shutdown_is_not_owner_loss_for_the_bound_handle` 与 `review_a_lost_owner_does_not_accept_another_drivers_exit`。

### R2-04 / P2 — 单一宿主入口的类型导出没有闭合

- 基线失败证据：`baseline-r2-04-exports.log` — `error[E0432]` 未解析 `FactoryRegistry`、`RUN_DEFINITION_FORMAT`、`RuntimeAssembly`、`RecoveryReason`、`StoreInvariant`。
- 修复：五个缺口 + 修复期自审新发现的同类缺口全部以 additive `pub use` 补齐：`StoreRevision`、`CapabilityConflictReason`、`InvalidId`，以及 ADR 0008 使宿主必须识别的 `LEGACY_RUN_DEFINITION_FORMAT_V1`；同时导出 R2-02/03 的新生命周期词汇（`DriverOwner`、`CompositionReleaseOutcome`、`CompositionReleaseRejection`、`DriverOwnerState`、`OwnerLossReleaseError` 已在位）。没有新增 facade crate，没有删除任何 inventory 承诺，没有把 internal 项升格为宿主面。
- 闭合证据：`fixtures/r2-host-fixture` 只依赖 `runtime-loader`、不是 workspace 成员（无法经 workspace 统一摸到子系统 crate），逐值显式类型：解析/组合/两种 plan 入口、两条 `RuntimeAssembly` 路径、声明式构造与两个格式 tag、driver 生命周期（含 `same_driver_as(exit.owner())`、`dispose_after_driver` 分支、`into_exit`）、owner-loss 释放（`RollbackFailure`/`CleanupResource` 穷尽匹配）、durable 事实（`StoreRevision`/`WorkflowReplayIdentity`）、`RecoveryReason`/`StoreErrorKind`/`StoreInvariant` 分类、各阶段互不塌缩的错误域、effect seam、store seam 以泛型 `S: DurableStore` 表达。`cargo check --manifest-path …` stable 与 `+1.85` 均 exit 0，clippy `-D warnings` exit 0。
- 排除项（连同理由写进 `review_r2_exports.rs` 模块文档与 inventory）：仅经方法推断到达的 inspection 类型（`Scope`/`CapabilityRegistry`/`CapabilityContext` 及深处的 `RegistryError`/`FiberError`/`EffectError`/`DependencyPin`/`EntryId`/`Generation`/`PluginFuture`）、legacy 配置词汇（`TaskConfig`/`EffectSpec`）、store 作者向词汇（`DurableMutation`/`CommitRequest`/`CommitResult`/`CommitLedgerEntry`/`IdempotencyKey`/`DurableJournal`/`JournalInvariant`/`classify_recovery`/`AttemptAdmission`/`EffectIntent`/`CancellationRecord`）留在 subsystem 层，与 ADR 0007 的闭合规则一致。

## 5. 测试计数（原测试与新增测试分开）

- 被审基线 `d3818f5` 实测执行：**324 passed / 0 failed（48 个测试二进制）**（在 `d3818f5` 的独立 worktree 中运行 `cargo test --workspace --all-features` 实测，非引用历史记录）。
- 修复 HEAD 实测执行：**342 passed / 0 failed**，stable 与 `+1.85` 一致（52 个测试二进制；doc-test 目标贡献 0 项）。差值恰为 +18，与新增断言一一对应；测试二进制 48 → 52 正是新增的 4 个回归文件。
- 声明的 `#[test]`/`#[tokio::test]` 标注数：310（集成 223 + lib 87）→ 328（集成 236 + lib 92），两种口径都列出：执行数高于标注数来自宏生成的用例（如 `crates/workflow-recovery/tests/durable.rs` 的 `test_both_backends!`，3 个标注展开为 12 个执行项），该差异在基线与修复 HEAD 上同量存在，因此 +17 的差值不受口径影响。
- 新增 18 项：`definition.rs` lib +5、`k6_r2_cleanup_interruption.rs` +4、`review_r2_probes.rs` +3、`k2_declarative.rs` +2、`k3_async.rs` +2、`review_r2_identity.rs` +1、`review_r2_exports.rs` +1。
- 既有断言：无删除、无 `#[ignore]`、无修改期望；`k4_*`/`k5_*`/`k6_*`/两个 example 只做 `into_released()` 形态迁移；被审的 `@` 分隔符守卫原样保留并通过。历史 K6 记录（`55bb80b` 的 315 项）作为历史事实保留，未被替换或重算。

## 6. 门禁逐项结果

| 命令 | 结果 |
| --- | --- |
| `cargo test -p runtime-core --test k2_declarative --test k3_async --test k3_owner_state --test k3_review_owner_drop` | exit 0（15 / 25 / 4 / 3） |
| `cargo test -p runtime-composition --all-features` | exit 0（37 项，含 4 interruption + 3 probes） |
| `cargo test -p runtime-loader --all-features` | exit 0 |
| `cargo test --workspace --all-features` | exit 0，342 passed / 0 failed |
| `cargo +1.85 test --workspace --all-features` | exit 0，342 passed / 0 failed |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo check --workspace --all-targets --all-features` | exit 0 |
| `cargo doc --workspace --all-features --no-deps` | exit 0，无 broken intra-doc link |
| `cargo run -p graph-lab` | exit 0（`m1: run=completed …`） |
| `cargo run -p runtime-loader --example r2_host` | exit 0（`R2 host lifecycle complete.`） |
| `cargo check/clippy --manifest-path fixtures/r2-host-fixture/Cargo.toml --all-targets` | stable 与 `+1.85` check exit 0；clippy `-D warnings` exit 0 |
| `git diff --check` | exit 0 |

**未运行 / 无法运行**：`cargo +1.85 clippy`（本机 1.85 toolchain 未安装 clippy 组件；MSRV 只验到 `check` 与 `test`）；远端 CI（未 push，GitHub Actions 未观测本分支）；`cargo package`（`publish = false`，非 R2 门禁）；power-loss/`fsync` 物理耐久、并发压力、Mirri/loom 式模型检查均未主张。构建仍有 Windows incremental cache 无法 hard-link 的环境警告，因此不写“零 warnings”。

## 7. 剩余风险与未做

1. **v1 durable run 无迁移路径**：只有 fail-closed 拒绝与typed 错误指路（ADR 0008）。若真实部署存在 v1 durable run，需要单独的迁移决策，本次不提供。
2. **`RuntimeAssembly::shutdown(&mut self)` 仍不可续跑**：它消费 assembly，取消即随 future 释放 runtime；这是记录在案的限制（ADR 0009 已列被否方案），可续跑的是两条 handle 路径。
3. **`OrderlySweepInterrupted` 之后的清理是 best-effort**：无法撤销的注册继续以失败上报，不当作顺序清理完成。
4. **fixture 证明“可命名”，不证明语义**：语义由 R2-02/03/01 的行为回归承担。
5. **MSRV 侧只有 `check`/`test`**，clippy 组件缺失；若复审要求，需在装有 1.85 clippy 的环境补跑。
6. **未引入 `unsafe`、未新建第二 authority**：ledger 的在飞步骤是 `Box<dyn Future>`（内存上界为单步），fiber 释放仍走 `CapabilityFiber::dispose`，registry 仍是唯一注册权威。
7. **未复审**：本报告不构成 APPROVE/Closed；K6 仍 Integrated-not-Reviewed，R2 未 Closed，Stage 3 入口门未打开。

## 8. 停止条件与下一步

按指令停止在 **READY FOR REVIEW / NOT INTEGRATED**：未 push、未 merge、未开始 Stage 3。下一步应由独立复审者基于 `d3818f5..HEAD`（`git diff d3818f5...HEAD`，撰写时为 `93d01c1`）与本报告 §6 的门禁清单进行；只有独立复审 PASS 且目标集成 HEAD 的 CI 证据齐备后，才单独形成 Stage 3 规划指令。