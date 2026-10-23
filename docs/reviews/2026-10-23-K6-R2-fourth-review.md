# K6/R2 第四次独立复审 — 2026-10-23

结论：**CHANGES REQUIRED（仅文档与证据，无代码阻断）**。6 项 medium、约 20 项 low，0 critical/high，0 代码缺陷。全部 finding 已在本轮修正（`45d38f3`），生产行为未变；剩余门禁是对该修正提交的限定范围确认。

被审 HEAD：`cb00662`。基线：`fe8e5ff`（第三次复审记录）。范围：`git diff fe8e5ff..cb00662`，2 个提交、26 个文件。
本报告不构成 APPROVE；K6 仍为 Integrated，R2 未 Closed，分支未合并。

## 1. 基线与方法

- OCR `delegate preview` 判定 14 个可审文件（543 插入），12 个文档类文件按规则排除后由子代理直接阅读。
- 5 个子代理分工：B1 闭合、B2/B3 武装与 panic、M4 `dispose_taken` 签名、M9/M10 身份与文档、L3/L4/L11/L12 文档一致性。其中闭合、武装/panic、身份三个子代理运行超时被中断，改由 3 个**只读替代者**（禁止构建）完成；主线程另做探针与变异实验。覆盖来源在 §6 明确标注，不把主线程自证当作独立结论。
- 关键变异（均在 %TEMP% 副本或临时改动后进行，仓库已复原）：去掉武装行、把 hook 构造改回先 take、把 capability id 改成 raw 拼接、从证据里删除名字、给 umbrella 加 glob。

## 2. 阻断项

**无。** 第三次复审的三个阻断项都已验证关闭：

- **B1** — 只依赖 `runtime-loader` 的探针现在可以命名 `CapabilityId`、`Generation`、`EntryId`（编译通过），五个 tiered 类型仍不可命名（5×E0425，符合设计）；fixture 仍只依赖 loader、仍非 workspace 成员，并在活代码中命名三者。
- **B2** — 去掉武装行后 `dispose_only_module_runs_its_hook_and_is_reported_clean` 失败（hook 0 次 vs 1 次）；恢复即通过。
- **B3** — 把 hook 构造改回 `take()` 之后调用，panic 回归失败（重试返回"完成"）；恢复即通过。panic 契约已写入 `CompositionHandle` 与 ADR 0009 §3。

## 3. 应修（medium，6 项）

1. `K6-supported-api-inventory.md` 的闭合规则仍写"命名 enum payloads it matches on"，与 ADR 0007 §2 及第六波（match-only 留 subsystem）冲突。
2. 同一文件的 tier 表仍把 `Generation`/`EntryId` 列在 subsystem、`CapabilityId` 未出现在任何 host 列表；`CapabilityGraphError` 仍带"umbrella 导出"★ 标记（实际未导出）。
3. ADR 0007 §2 的新"按种类分层"规则与既有 payload 导出（`CapabilityConflictReason`、`PluginConflictReason`、`FactoryConflictReason`、`ActivationStage`、`ConstructionStage`、`StoreInvariant`、`RecoveryReason`）之间缺少边界说明。
4. `K4-runtime-plugin-composition.md` 与 `assembly.rs` 的注释仍写"dispose hook 只在 activate 完成时运行"，与 B2 修复后的武装条件相反。
5. `COMPATIBILITY.md` §1 未记录 `dispose_taken` 的公开签名变更（`(plugin, &runtime)` → `(&runtime)`）。
6. handoff 与计划把 346 写成"test-result 行数"（实际 52 条 result 行、346 个用例、341 个独立父级用例）。

## 4. 低优先（约 20 项，摘要）

- `export_coverage.rs`：注释可满足检查；glob 被当成字面量 `*` 接受；死代码别名让"覆盖"退化为"被 import"（前两项已修并验证）。
- `assembly.rs` 的 §Panics 首句与第二句矛盾；ADR 0009 §3 缺 panic 契约（已补）。
- `ActivatedModule`/`run_sweep` 注释陈旧（已改）。
- 我引入的错误：`RuntimeError::FactoryResolution` 不存在（应为 `RuntimeError::Factory(FactoryResolutionError::MissingFactory)`），ADR 0008 §3 同错（均已修）。
- ADR 0008 §1 夸大 skeleton 的覆盖；`renamed` 断言与金样重复、非独立证据（已限定措辞并注明）。
- 契约当前状态仍写 K6 review pending；README/ROADMAP 未反映第三次复审；契约"preserve the exit"未限定变体（`OrderlySweepInterrupted` 无 exit）；repair report 的 48/52 单位错；handoff 仍称注册清理"不可达"。
- `remove` 文档未说明取消后条目已删、不可重试、剩余 fiber 随 runtime 丢弃；等价性测试只比较 Display（已改为比较 payload）。
- fixture 不在 workspace 的 fmt 门禁内（已格式化）。
- `CapabilityId` 是 `Id` 的别名，"原本不可命名"的表述过强（inventory 已注明）。

## 5. 已修正

`45d38f3` 修正了上述可在同一轮处理的全部项（§8 记录了对该提交的补充修正）：仅文档、注释、测试证据与 fixture 格式，无生产行为变化。复跑门禁：fmt 干净；clippy（stable 与 1.85）干净；`cargo test --workspace --all-features` 350 passed / 0 failed（53 条 result 行：40 个测试二进制、8 个 doc-test、5 条子进程重跑）；`cargo doc` 干净；fixture check/clippy 通过。

## 6. 覆盖率与方法说明

- 14/14 可审文件覆盖（100%）。
- 独立结论来源：M4 与文档一致性两个完整子代理；闭合、武装/panic、身份三个只读替代者。
- 主线程自证（不计入独立结论，仅作证据）：4 项变异、2 个仓库外探针（loader-only 命名、注册清理可达性）。
- 未覆盖：K1–K5 历史代码、`docs/research/**`、Stage 3 设计。

## 7. 判定与下一步

- 对 `cb00662` 的判定：**CHANGES REQUIRED**（文档/证据层面，无代码阻断）。
- 修正后已 `--no-ff` 合并到 `main`（`53bd0cc`，CI run 39888844869 绿）。
- 最终限定范围确认（对 `13f5f6c`）以一条 low 文档描述为条件返回 PASS；该条已在 closeout 提交中按确认者的建议修正。
- K6 转 Reviewed、R2 转 Closed；合并后不再改动生产代码。

## 8. 限定范围确认（2026-10-23）

对 `45d38f3` 的只读确认发现 1 项 medium 与 4 项 low：

- medium：契约、README、ROADMAP 的状态段落仍把第四次复审判为 pending（落后一次复审）。
- low：ADR 0007 对"注释/glob"的描述与实现不符；花括号内的 glob 未被拒绝；2026-09-28 报告残留 `EntryId`/`Generation` 的 subsystem 表述；等价性测试仍比较 `Display` 而非 payload。

以上已在紧随本记录的修正提交中处理，门禁复跑保持绿色。至此第四次复审的全部 finding 均已关闭；下一步是 `--no-ff` 合并、观测 `main` CI、写集成证据并完成 K6/R2 closeout。

## 9. 最终确认与结论（2026-10-23）

对 `13f5f6c` 的只读最终确认：状态文本（README/ROADMAP/契约/计划）、第四复审记录、export gate（`//` 与 `/* */` 注释剥离、bare 与花括号内 glob 均拒绝）、350/53 与 346/52 的计数、`dispose_taken` 最终签名均核对通过。唯一未闭合项是 ADR 0007 §2 对注释处理的描述（low）；确认者明确"改掉后即可 PASS"，并建议在记录 PASS 的 closeout 提交中一并修正——该修正已按原文执行。

**最终结论：PASS（0 blocker）。** 第三次复审的三个阻断项、第四次复审的 6 项 medium 与全部 low 均已关闭；K6 记为 Integrated / Reviewed，R2 记为 Closed，Stage 2 完成。closeout 的状态更新是对本结论的转录，不是一次新的复审。
