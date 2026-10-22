# K6/R2 第三次独立复审 — 2026-10-22

结论：**CHANGES REQUIRED / R2 NOT CLOSED**。3 项阻断、10 项应修、12 项低优先，另驳回 1 项。
R2-01（身份 v2）与 R2-02（owner 绑定）的修复成立；R2-03 的取消续跑机制成立，但"模块只在全部清理成功后才进入 cleaned"这一完成契约仍有一处静默丢失；R2-04 的"闭合"主张不成立，闭合证明本身是白名单式的。

被审 HEAD：`3ad6fba`（工作树干净）。基线：`d3818f5`。范围：`git diff d3818f5..HEAD`，21 个提交、58 个变更文件。
本报告不构成 APPROVE；K6 仍为 Integrated，R2 未 Closed，repair 分支未合并。

## 1. 范围与方法

- OCR `delegate preview`（`--from d3818f5 --to HEAD`）判定 58 个变更文件中 30 个可审（8653 插入），28 个文档类文件按规则排除；`delegate rule` 对 30 个文件返回同一组规则（错误处理与 panic、异步取消安全、所有权与生命周期、并发与共享状态、类型/API 设计、安全敏感代码）。
- OCR 只做确定性的文件筛选与规则解析；**判断由 7 个子代理独立完成**：6 个代码子代理（身份 v2、驱动 owner、组合清理核心、回归测试有效性、注册表拆分与标识符分配、loader 表面与 fixture）与 1 个文档/CI 子代理。每个子代理只拿到文件范围与契约问题，不共享主线程的结论。
- 主线程只做三件事：核对每个 finding 到具体代码行、做变异实验、写仓库外探针。所有 26 个代码 finding 都经过复核；1 条被驳回（§6）。
- 证据在 [evidence/r2-2026-10-22/](evidence/r2-2026-10-22/)：注册清理可达性探针（HEAD 与 `0ec8096` 两次运行日志）、宿主词汇闭合探针（9 个 E0425）。

## 2. 阻断项

### B1 / high — R2-04 的"闭合"主张不成立，闭合证明是白名单

位置：`crates/runtime-loader/src/lib.rs`、`fixtures/r2-host-fixture/src/lib.rs`、`docs/runtime/K6-supported-api-inventory.md`。

ADR 0007 §2 的规则是"宿主在受支持路径上必须能**命名**它收到的每个类型：它匹配的枚举 payload 与它绑定的值"。实测（两个独立 probe，证据 `closure-probe/check.log`）以下类型只依赖 `runtime-loader` 时不可命名：

- `ScopeError::GraphAdmission` / `DefinitionError::CapabilityGraph` / `ReconstructionError::CapabilityGraph` 的 `CapabilityGraphError`；
- `ScopeError` 多个变体携带的 `CapabilityId`；
- `JournalError::InvariantViolation` 的 `JournalInvariant`；
- `StoreError::IdempotencyConflict` 的 `IdempotencyKey`；
- 已导出的 `CapabilityPin` 的公开字段类型 `Generation`/`EntryId`，以及 `CapabilityHandle::id()` 的 `CapabilityId`；
- 已导出的 `Runtime::workflow()`/`journal()` 返回的 `WorkflowGraph`/`DurableJournal`（inventory 未对其作分层声明）。

同时，fixture 无法发现这些缺口：它从不匹配上述变体、不读 `CapabilityPin.generation`；loader 导出的 `CapabilityContribution`、`CapabilityFactoryFn`、`ModuleRegistrationFactory` 在 fixture 与全部 tests/examples 中零引用（`CapabilityOwnership` 仅 1 处），删掉这三个 `pub use` 两个"证明"仍全绿。因此"closure is proved structurally"超出了证据：它证明的是**被选中的名字**可命名。

注意：这些缺口在 `d3818f5` 已存在，不是本分支引入；但本分支把"闭合已结构化证明"写进了 README、计划与 inventory，因此主张失真。R2-04 的字面五项确实已补。

修复要求（二选一，且必须让证明能失败）：
1. 补齐宿主面：导出 `CapabilityId`、`Generation`、`EntryId`、`CapabilityGraphError`、`JournalInvariant`、`IdempotencyKey`、`WorkflowGraph`、`DurableJournal`（或明确这些访问器为子系统层并从宿主面移除）；
2. 或收窄主张：inventory 逐类型分级（`CapabilityId` 现在整份清单一次都没出现），并把 fixture 改为可被反向验证的形式——例如一个检查"`lib.rs` 的每个 `pub use` 名字都在 fixture 中被命名"的测试，否则删除导出即无法察觉。

### B2 / high — 只注册 `on_dispose` 的模块其钩子永不执行，却计入 `cleaned`

位置：`crates/runtime-composition/src/plan.rs:319-327`（武装条件）、`src/assembly.rs:175`（`if hook_armed`）与 `finish_current`。

`hook_armed` 只在"存在 `on_activate` 且它成功返回"时置位；宿主合法地只注册 `on_dispose`（`registration.rs` 承诺它在模块资源释放时运行，未要求同时注册 activate）时，dispose 钩子从不进入 `current_hook`，`run_sweep` 直接完成该模块并写入 `report.cleaned`，`failures` 为空。这与 `assembly.rs` 自己的"a module enters cleaned only when all of its owned cleanup succeeded"以及 R2-03"不得静默丢失步骤"直接冲突。

证据：子代理仓库外探针，只注册 `on_dispose` 的模块走 owner-loss 释放 → `hook_starts=0 cleaned=[dispose-only] failure_count=0`；仓库现有 `on_dispose` 站点都同时注册了 activate，所以测试套件看不到。

根因在未改动的 `plan.rs`，但完成契约是本次修复重写的，因此本轮必须定案：把"激活成功"（与是否存在 activate 钩子无关）作为武装条件，并补一条只注册 `on_dispose` 的回归；或明确写成已知限制并从 `cleaned` 契约里排除。

### B3 / medium — panic 语义未定义：构造期 panic 丢步骤、poll 期 panic 毒化账本

位置：`crates/runtime-composition/src/assembly.rs:294-298`（先 `take` 再 `hook()`）、`:276-283`（`await_parked` 返回后才清 `in_flight`）。

- `hook()` 是宿主闭包，调用本身可能 panic：展开时 `current_hook` 已 `take`、`in_flight` 尚未写入，钩子丢失；宿主 `catch_unwind` 后重入会把该模块计入 `cleaned` 且无失败记录。
- 已 park 的步骤 future 在 poll 中 panic 时，`in_flight` 仍保留该 future；重入再次 poll 会触发标准库的 `` `async fn` resumed after panicking `` 硬 panic，账本从此不可用，剩余资源只能随 handle 丢弃。

两条都不涉及 unsafe，但要与文档对齐：`assembly.rs` 与 ADR 0009 都承诺"取消后可重入"，没有说明 panic 是不可恢复的。最低要求：明确 panic 语义（禁止重入/毒化），并把步骤记为 failure 而不是报告完成；更进一步用 `catch_unwind(AssertUnwindSafe(..))` 包住 `hook()` 构造与 in-flight await，panic 后丢弃该 future 并记录失败。

## 3. 应修（medium）

| # | 位置 | 问题 | 证据 |
|---|---|---|---|
| M1 | `runtime-loader/src/lib.rs`、`runtime-core/src/lib.rs` | `CapabilityPin` 的公开字段与 `CapabilityHandle` 访问器返回 `Generation`/`EntryId`/`CapabilityId`，三者不可命名；与 fixture "逐值显式类型"的前提冲突 | probe `E0425`×3 |
| M2 | `runtime-core/src/lib.rs`（`Runtime::workflow`/`journal`） | 返回 `WorkflowGraph`/`DurableJournal`，两者未导出且 inventory 未分级 | probe `E0425`×2 |
| M3 | `assembly.rs:632-640` | 外来 exit 的 rejection 以 future 形式返回并拥有 exit；调用方在首次 poll 前 drop（提交信息自己举的 `select!` 场景）会销毁外来 exit，其原组合随后只能得到 `OrderlyShutdownCompleted`，全权清理永久不可达 | 子代理探针 |
| M4 | `capability-graph/src/semantic.rs:601-621` | 公开 `dispose_taken` 不要求先 `take`、不校验 `plugin == runtime.id()`；任何持有 Arc 的调用方可提前处置仍在注册表的 runtime，失败归因可能指错插件；`take` 的 `must_use` 文案"the only handle left"与 `get`/plan 仍持 Arc 不符 | 代码核对 |
| M5 | `runtime-composition/tests/review_r2_probes.rs:157-172` | "零副作用"三条断言只在 rejection 返回后才执行；把 owner 校验去掉后 sweep 卡在未释放的 Gate 上，三个用例各自挂死（25s+ kill），没有任何断言失败；CI 上表现为 30 分钟超时 | 子代理变异实验 |
| M6 | `tests/k6_r2_cleanup_interruption.rs:44-62`、`review_r2_probes.rs:42-61` | Gate 先 `load(opened)` 再写 waker，存在丢唤醒窗口；只有一个 waker 槽；`release()` 实际只唤醒 noop waker，唤醒通道是死代码。单线程掩盖了它 | 代码核对 |
| M7 | `docs/runtime/STAGE2-MILESTONE-PLAN.md` 2026-10-22 段落、`docs/reviews/2026-10-22-*.md` | "346 passed / 52 binaries"口径错误：52 是 `test result` 行数（39 个测试二进制 + 8 个 doc-test 目标 + 5 条测试内子进程重跑），346 含 5 条子进程重跑，独立父级用例是 341 | 实测：39 Running 行 / 52 result 行 |
| M8 | `docs/architecture/0009-driver-exit-owner-binding.md` §3、`docs/reviews/2026-10-22-K6-R2-third-review-handoff.md`、计划同段 | "defensive rather than currently reachable"不成立：宿主经 `RuntimeAssembly::runtime() → capability_registry().instantiate(..)` 可为同一插件创建未记入模块 fiber 列表的 fiber，注册清理步随即出现真实等待点。同一序列在 HEAD 通过、在 `0ec8096` 失败 | [probe-head.log](evidence/r2-2026-10-22/probe-head.log) vs [probe-prefix-0ec8096.log](evidence/r2-2026-10-22/probe-prefix-0ec8096.log) |
| M9 | `runtime-core/src/lib.rs:762-768` | `factories.validate` 在 `store.load_run` 与身份 gate 之前：缺任一 factory 时返回 `MissingFactory`，legacy 身份的 `LegacyDefinitionIdentityUnsupported` 不可见；身份判定发生在一次 durable 读之后 | 子代理探针 |
| M10 | `runtime-core/src/definition.rs:1252-1283`、`:21-24`、ADR 0008 §1 | skeleton oracle 只变动 value 槽位，id 字段恒定，`assert!(identity.contains(']'))` 恒真；注释/ADR 称"every field is tagged"，实际只有分组与枚举 tag，交换 `capability` 的 id/kind/definition_identity 会静默改变全部 identity 而格式 tag 不变 | 代码核对 |

## 4. 低优先（记录，不阻断）

- L1 `async_driver.rs:274-282`：`DriverOwner` 的 `Debug` 打印 `MailboxState` 堆地址，与"no observable payload"矛盾；k3 的不透明性测试只禁了若干字符串，`0x…` 能通过。
- L2 `runtime-core/tests/k3_async.rs:1426-1472`：两个 driver 用不同 `RunId`，按 RunId 等值的退化实现也能通过；同 RunId 场景只在组合层覆盖。
- L3 `R2-KERNEL-CONTRACT.md:219-231`：称同一 handle 再次 `dispose_after_driver` 会得到 `OrderlySweepInterrupted`，该分支不可达；若到达还会丢弃调用者新传入的 exit（该 rejection 不携带载荷）。
- L4 `error.rs:614-617`：`AlreadyReleased` 的 Display 仍写"owner-loss release already ran"，而语义已改为"整个清理流程完成一次"。
- L5 `k6_r2_cleanup_interruption.rs:456-473`："best-effort says so"的用例中 solo 模块没有注册项，两种 authority 下断言相同；它实际证明的是 `orderly_release_accepted` 抬起了 `OrderlyShutdownCompleted` 守卫。
- L6 `semantic.rs:611-613`：`dispose_taken` 的"never the runtime"是调用方义务，借用签名不提供该保证（真正的保活来自 handle 的 ledger）。
- L7 `semantic.rs:576-580`：`remove` 的文档称服务"不能因取消而丢失 disposal"的调用方，但它内联 await、取消即丢失。
- L8 `capability-graph/tests/semantic.rs:900-908`：等价性测试只比较 Display 文本，只对 split 侧匹配变体。
- L9 `capability-graph/src/runtime.rs:253-266`：CAS 分配器没有首值契约与并发唯一性测试（把返回值误写成 `next` 不会失败）。
- L10 `runtime-loader/examples/host_end_to_end.rs:144-161`：不检查 `rollback.is_success()`，dispose 失败仍以 0 退出并打印 `shutdown status: Clean`。
- L11 `fixtures/r2-host-fixture/src/lib.rs:255-270`：把任意 rejection 重包成 `AlreadyReleased`，作为"标准词汇"示例会被照抄。
- L12 文档细节：README 说"the two commits after it"（实为三个）；`K2-declarative-cold-reconstruction.md:13` 写"unmarked as Integrated"（应为 Reviewed）；README "does not include a plugin loader" 与已集成的 K5 冲突；`2026-09-28-K6-R2-repair-report.md:86` 的 +17 应为 +18；handoff 说本地验证在"a108bee 之上、仅文档变更"（实含 `c2870a8` 测试提交）；`R2-KERNEL-CONTRACT.md:48-54` 仍写 K6 review pending（已发生 CHANGES REQUIRED）；`ci.yml:13` 的 push run 永不取消对排队中的 run 不成立。

## 5. 五个焦点逐条结论

1. **同步 acceptance：成立。** owner 校验、账本标记与 future 构造之间没有 await/锁/宿主代码；`&mut self` 借用挡住并发重入。变异实验（把 acceptance 移进 `orderly_sweep` 首次 poll）只让 pre-poll 用例失败，报 `OrderlyShutdownCompleted`，正是预期的捕获点。
2. **可 park 的注册清理：机制成立，且是承重的，不是防御性。** 见 M8 与证据日志：HEAD 通过，`0ec8096` 报"registration could not be unregistered"。文字表述需要修正，代码不需要改。
3. **身份 v2：成立。** 6246 个通过 `validate()` 的对抗定义 0 碰撞；token 三类首字节互斥、`%len:` 字节定界可唯一解码；多字节金样能抓住字符计数错误；v1 canonicalizer 只在 `#[cfg(test)]`，生产路径只比较与导出常量。但"逐字段 tag / 重排安全"的注释与 ADR 措辞超出实现（M10），legacy gate 顺序有 M9。
4. **CI 门禁：真实。** 9 个 run id 的结论与 head 全部核对一致；`b725eda` 有 `toolchain` 输入、`ccc8c1f` 写死 1.85.1；`RUSTUP_TOOLCHAIN=1.85.1` 确实压过 `rust-toolchain.toml` 的 stable，日志里是 `cargo 1.85.1`；MSRV clippy 与 fixture 三步都在日志中。只有 `ci.yml:13` 的注释夸大（L12）。
5. **文档真实性：不成立的地方已列。** 历史段落"被同文档新段落取代"的规则执行正确；失实集中在 2026-10-22 段落的"不可达"、验证口径（M7）、以及对闭合的表述（B1）。

## 6. 被驳回的 finding

子代理 D 报告"pre-poll 用例在修复前的实现上也会通过，因此不能作为 R2-03 的回归证据"。复核后驳回：该结论来自把实现改回 `d3818f5` 的 `self.owned.take()` 语义（另一个设计），而 0ec8096 修的是**接受时机**。把 acceptance 移进首次 poll（同一签名下的等价变异）会让该用例在 `release_after_owner_loss` 处失败并报 `OrderlyShutdownCompleted`，与该 bug 的症状一致。子代理的另一个观察成立：`count == 0` 那两条断言在惰性 future 下恒真，属于冗余而非缺陷。

## 7. 覆盖率

- OCR 可审文件 **30/30 已审**（coverage 100%），无跳过；其中 25 个产品/CI 文件由 6 个代码子代理覆盖，5 个 `docs/reviews/evidence/r2-2026-09-28/` 下的历史探针由主线程与仓库内对应测试逐份比对（内容为第一次复审的最小反例，非产品代码）。
- OCR 排除的 28 个文档/锁文件由文档子代理逐份阅读；`fixtures/r2-host-fixture/{src/lib.rs,Cargo.toml}` 虽被 OCR 排除，由 loader 子代理直接阅读并实测。
- 复审未覆盖（按范围声明）：K1–K5 的全部历史代码、`docs/research/**`、Stage 3 设计。

## 8. 判定与下一步

修复顺序建议：

1. **B1**：决定"补导出"还是"收窄主张"；无论哪条，都要让闭合证明可被反向验证（删除一个受保护的导出必须变红）。
2. **B2**：改 `hook_armed` 的武装条件并补只注册 `on_dispose` 的回归；或写进已知限制并同步 `cleaned` 契约。
3. **B3**：写出 panic 语义并让 panic 不再产出"cleaned"；能加 `catch_unwind` 更好。
4. **M1–M10**：按表中顺序，M7/M8 属于"文档说了假话"，必须与本轮一起改，不能留到下次。
5. 低优先项可随修复顺手处理，L3/L4/L11 是同一处措辞问题，建议一次改完。

修完后应做**第四次复审**，范围限定为 `3ad6fba..修复HEAD` 加上 B1/B2 触及的契约段落；只有 0 blocker 才能合并并把 K6 转 Reviewed、R2 转 Closed。

在阻断项关闭前不要合并，也不要把本报告当作"已复审通过"。
