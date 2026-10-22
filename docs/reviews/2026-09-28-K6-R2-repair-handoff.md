# K6/R2 下一步执行指令 — 有界修复包

状态：**NOT_STARTED**。依据 [2026-09-28 独立复审](2026-09-28-K6-R2-rereview.md)。目标只是在当前 R2 内关闭 R2-01～R2-04，达到 **READY FOR REVIEW / NOT INTEGRATED**；本指令不是 Stage 3 启动令。

## 可直接交给执行者的任务

在 `F:\Workspace\kernis`，以 `d3818f5d2a77030f353e1a2a6c1744d494cae0c4` 为复审基线，先复现报告中的四项问题，再按下列顺序形成兼容性决策、实施修复、验证和交付。

你不是工作区唯一使用者。开始先核对 HEAD、分支、dirty files；保留 `.workbuddy/` 和其他人的工作。本次 `docs/reviews/` 是评审产物，也要保留。单写者工作，遵循现有 AGENTS.md 顺序执行。代码修复在独立 `codex/` 分支或适合的隔离 checkout 上完成；若 HEAD 已变化，先核对报告对应问题是否仍存在，不能盲套旧行号。

不要 push、merge、发布、自动启动 Stage 3，也不要将实现者的绿色测试写成独立 APPROVE。

## 精确文件范围

主要实现：

- `crates/runtime-core/src/definition.rs`：结构无歧义的定义身份编码。
- `crates/runtime-core/src/lib.rs`：restore 的版本/兼容性检查、typed error、re-export；保持 provenance 边界。
- `crates/runtime-core/src/async_driver.rs`：DriverExit 与其 driver owner 的不可伪造绑定；保留现有 mailbox 作为 owner 生命周期事实来源。
- `crates/runtime-composition/src/assembly.rs`：正常清理入口校验及中断安全的清理账本。
- `crates/runtime-composition/src/error.rs`、`src/lib.rs`：typed rejection 与 exports。
- `crates/runtime-loader/src/lib.rs`：完整 host vocabulary exports。

测试/调用点：

- `crates/runtime-core/tests/k2_declarative.rs`、`k3_async.rs`、`k3_owner_state.rs`、`k3_review_owner_drop.rs`。
- `crates/runtime-composition/tests/k4_cleanup_ownership.rs`、`k4_compat.rs`、`k4_composition.rs`；允许增加聚焦回归文件。
- `crates/runtime-loader/tests/k6_r2_end_to_end.rs`，新增仅依赖 loader 的宿主编译 fixture。
- `crates/runtime-loader/examples/r2_host.rs`、`host_end_to_end.rs` 及仓库内 `dispose_after_driver` 直接调用点，限必要的 API 迁移。
- `crates/workflow-recovery/` 仅在身份兼容 ADR 证明必须调整持久化或错误导出时进入范围；禁止顺手重写 redb/物理格式。
- `crates/capability-graph/` 默认不改；如果清理中断修复必须深入 fiber disposal，先记录所有权证据和最小范围，禁止另建 lifecycle authority。

文档：新增身份兼容 ADR（先检查下一个可用编号）；更新 `docs/runtime/{COMPATIBILITY.md,K6-supported-api-inventory.md,R2-KERNEL-CONTRACT.md,STAGE2-MILESTONE-PLAN.md,K2-declarative-cold-reconstruction.md,K3-explicit-async-boundary.md,K4-runtime-plugin-composition.md}`、ADR 0005/0007、README/ROADMAP 的必要状态及迁移说明；增加一份修复结果报告。

## 1. 复现与固定验收基线

先读复审报告和证据源文件。将 `docs/reviews/evidence/r2-2026-09-28/review_r2_identity.rs`、`review_r2_probes.rs`、`review_r2_exports.rs` 分别放入 runtime-core、runtime-composition、runtime-loader 的 `tests/`，如同名文件存在先检查，不覆盖别人的文件。composition 反例使用现有 `k4_common`。

```powershell
rtk cargo test -p runtime-core --test review_r2_identity -- --nocapture
rtk cargo test -p runtime-composition --test review_r2_probes -- --nocapture
rtk cargo test -p runtime-loader --test review_r2_exports --no-run
```

在被审基线上预期分别是 1 failed、2 failed、E0432。这些是待修缺陷的反例，不是既有工作区测试已坏。记录实际值；如果与报告不同，先定位版本或环境差异。修复时将其整理进正式回归，不以删除断言、ignore 或修改期望为“修复”。

## 2. 先关闭身份兼容性决策，再实施 R2-01

这一步必须先于修改格式常量。当前契约冻结 `kernis-run-definition-v1`，而碰撞意味着仅凭旧身份无法知道原始定义。先提交可评审 ADR，明确：新写入格式、旧 v1 读取策略、歧义数据拒绝策略、迁移所需证据、CAS/重复执行语义和 breaking change 说明。

推荐决策：新定义使用显式版本且结构无歧义的编码（例如 v2，字段 tag + 集合数量 + 每个字符串的字节长度）；旧 v1 字节算法保留为历史解释，不能静默更换其输出。旧 `kernis-workflow-replay-v1` 不受影响。不要 hash 当前有歧义的串，也不要给少量保留词打补丁。

对于旧 declarative v1，默认采取 typed fail-closed：在无法证明原始语义时，拒绝自动恢复/迁移。若希望保留一部分 v1 自动恢复，ADR 必须给出可证明不歧义的判定规则及反例测试；仅比较当前输入重算的旧身份不算证明。任何迁移只能使用受信任的原始定义和显式授权，以 durable CAS 防止 A→B→A 陈旧写入；本修复包不自动执行真实数据迁移。

这是会影响既有 R2 兼容承诺的决策。**若任务授权尚未选定旧 v1 处置政策，先完成 ADR 提案并在这个具体决策点停下，不擅自废弃已有数据兼容性。** R2-02～04 可独立继续，但最终 R2-01 和 R2 closeout 保持未完成。此限制来自 Stage 2 plan 的 compatibility/ADR 规则及 COMPATIBILITY.md §2，不是要求对每个普通代码修复重复确认。

决策确定后验证：

- 本报告 A/B 反例的身份不同、错误定义恢复被拒绝，且失败前零 factory 调用/零 durable 写入。
- 覆盖合法字段包含 `tasks/capabilities/dependencies/effect/present/absent`、冒号、数字、Unicode，组合边界仍可区分；不限于当前反例。
- 排序变化和 label-only 变化仍保持语义等价；拓扑、能力要求、effect semantics 改变必须不同。
- 固定 golden canonical bytes；不能只比较同一实现两次运行。为新旧格式建立明确 read/write matrix。
- 旧 live-object identity fixture、declarative legacy-mutator 拒绝、revision CAS、跨进程恢复全部保持正确。

## 3. 修复 R2-02：正常退出身份也必须绑定

在 DriverExit 中携带与 RuntimeHandle / CompositionHandle 对应的私有 owner 身份，沿真实 driver 创建和退出路径传递。复用现有 mailbox 身份/私有共享 token 的对象身份即可，不引入全局 owner 注册表，不以 RunId 代替实例身份，不暴露可伪造的 bool/string proof。

`dispose_after_driver` 必须在任何 hook/fiber/registry 操作前校验同源性。错误应 typed，并保留所有资源和传入 exit 供正确路径继续；可调整返回型或借用方式，但先给出 API 迁移说明并更新全部调用点。不能只在 owner_state 为 Running 时拒绝，因为两个已经 Shutdown 的实例仍可串线。

回归矩阵：A Running + B exit；A Shutdown + B exit；同 RunId/同配置不同实例；A owner-loss + B exit；拒绝后 A/B 各自正常清理；正确来源只清理一次。给 A/B 使用相同 plugin ID，验证错误路径没有移除任何一方 registry 项，hook/fiber 计数均为零。

## 4. 修复 R2-03：清理中断不能遗失资源和事实

将清理进度与尚未完成资源保留在可再次访问的 owner 中。不要在首次 await 前将整个 ledger 移入一个可被取消的临时 future；`cleanup_modules` 在 await 前 pop 当前 module 的步骤也要纳入设计。

定义 pending hook/fiber 清理的归属和再进入语义：优先保留同一个进行中的清理 future，并在下一次调用继续 poll；不能重新触发已经开始的非幂等 hook。如果无法继续，必须保留结构化未完成状态及剩余资源，不能用 `AlreadyReleased` 表示完成。只有整个清理流程真实完成后才进入最终状态。

测试至少在 dispose hook Pending 和 fiber disposal Pending 两处取消；重入后未开始的模块继续清理，已开始步骤不重复启动，报告保留已发生失败，成功完成后第二次调用才拒绝。继续验证 reverse-order、单项失败继续、registry authority 丢失如实报告、正常 shutdown 不降级 owner-loss。

## 5. 修复 R2-04：显式类型覆盖宿主入口

以 inventory 和 ADR 0007 为起点，列出支持路径返回类型、必需参数、公开 enum payload，逐项决定 export；至少关闭已证明的五项缺口，并继续检查 StoreError/RecoveryDecision/DefinitionError 的传递分类类型。

增加一个真正只声明 `runtime-loader` 为生产依赖的宿主 fixture，显式命名 RuntimeAssembly、构建 FactoryRegistry 路径、匹配恢复 reason 和 store invariant，并覆盖身份与生命周期修复引入的新增错误/返回类型。现有示例依靠类型推断可运行，不足以替代该 fixture。

保持 additive exports 和当前层次；不新建 facade crate，不引入第二个 host entry，不通过删除 inventory 承诺让测试“通过”。

## 6. 分层验收与交付

| 验收项 | 必需结果 |
| --- | --- |
| R2-01 编码与错误定义恢复 | 反例不再碰撞；拒绝发生在 effect/factory/durable 副作用前；版本兼容 ADR 已确定 |
| R2-02 同 owner 清理 | 跨 owner typed reject、零清理、资源可继续用于正确退出 |
| R2-03 中断恢复 | 无遗失 ledger，无重复 hook，无虚假 AlreadyReleased/成功报告 |
| R2-04 host exports | 独立 loader-only fixture 编译通过、支持 payload 可匹配 |
| 既有语义 | K1/K2/K3/K4/K5/K6 原测试通过，断言含义未削弱 |
| 状态文档 | 准确区分实现、独立 review、集成和远端 CI；本次 finding 都有 evidence 链 |

先运行聚焦 tests，问题收敛后执行一次完整门禁：

```powershell
rtk cargo test -p runtime-core --test k2_declarative --test k3_async --test k3_owner_state --test k3_review_owner_drop
rtk cargo test -p runtime-composition --all-features
rtk cargo test -p runtime-loader --all-features
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets --all-features -- -D warnings
rtk cargo check --workspace --all-targets --all-features
rtk cargo test --workspace --all-features
rtk cargo +1.85 check --workspace --all-features
rtk cargo +1.85 test --workspace --all-features
rtk cargo doc --workspace --all-features --no-deps
rtk cargo run -p graph-lab
rtk cargo run -p runtime-loader --example r2_host
rtk git diff --check
```

独立宿主 fixture 使用其实际 manifest path 单独 `cargo check --manifest-path ...`，stable/MSRV 均要验证。不要用 workspace 隐式依赖或跳过编译验证替代。

新增 `docs/reviews/` 修复报告，记录 base/final HEAD、文件范围、兼容 ADR、四项 finding 对应修复和失败→通过证据、原测试与新增测试分开计数、未运行项、剩余风险。Stage 2 plan 追加此次复审 CHANGES REQUIRED 与修复记录，保留历史事实；独立复审尚未进行时不能写 APPROVE/Closed。

停止条件：四项实现验收完成且完整门禁通过后，标记 **READY FOR REVIEW / NOT INTEGRATED** 并交付。若身份政策未确定，明确 **R2-01 BLOCKED ON COMPATIBILITY DECISION**，交付可评审 ADR和其他已完成修复；不伪造全包完成。不 push/merge，不自动开始下一里程碑。后续独立复审 PASS + 目标集成 HEAD CI 证据齐全后，再单独形成 Stage 3 规划指令。
