# Workspace Checkpoint 与 Git 回滚

`defect-agent` 里的 `prompt_turn` 需要一个文件系统级的回滚基线，但这个基线不应污染用户分支历史。本文定义一层**内部 checkpoint**：利用 git 对 workspace 做快照，但只写隐藏引用，不向当前分支制造可见提交。

## 1. 目标

- **可回滚**：能把某个 `prompt_turn` 期间的文件改动回退到 turn 开始前。
- **不污染用户历史**：不把 agent 的内部 checkpoint 直接提交到用户当前分支。
- **可审计**：checkpoint 必须可追溯到 session / turn。
- **可组合**：和 session rewind 分开，但可以共享同一个 turn 边界。

## 2. 非目标

- 不实现 workspace 的“任意时间点语义回滚”。
- 不把文件回滚和 session history 回滚绑成一个原子操作。
- 不承诺处理所有 git 边界情况的全自动恢复，例如复杂 submodule、LFS、受保护分支策略冲突。

## 3. 核心判断

直接把 `prompt_turn` 变成用户分支上的 `git commit` 不合适：

- 会污染用户提交历史。
- 会把用户未授权的改动混进来。
- 会让 rollback 语义和 `git reset --hard` 绑定，风险过高。

正确方向是：

1. `prompt_turn` 开始时创建内部 checkpoint。
2. checkpoint 只写到隐藏 ref。
3. rollback 时只恢复 workspace，不移动用户分支。

## 4. Checkpoint 形态

推荐把 checkpoint 记录成一条隐藏的 git 对象引用，形如：

```text
refs/defect/checkpoints/<session_id>/<turn_seq>
```

每个 checkpoint 至少记录：

- `session_id`
- `turn_seq`
- `base_head`：创建 checkpoint 时的当前 `HEAD`
- `tree_oid`：workspace 的快照 tree
- `parent_checkpoint`：前一个 checkpoint
- `created_at`
- `cwd`

如果需要更强的可恢复性，可以把这些元数据再落一份到 session 目录里的 JSON 索引，但真正的文件快照仍以 git 对象为准。

## 5. 创建时机

checkpoint 的粒度是**一个 prompt turn**，不是每个 LLM 调用，也不是每个 tool。

建议时机：

1. 用户提交 prompt，turn 即将开始。
2. 如果 workspace 在 git repo 内，先创建 checkpoint。
3. 然后才允许该 turn 的文件改动进入 workspace。

这样 rollback 的语义清楚：回到这次 prompt 之前的 workspace 状态。

## 6. 捕获内容

建议默认捕获：

- tracked 文件的修改
- 新建的 untracked 文件
- 删除操作

默认不捕获：

- ignored 文件
- git 子模块内部的独立历史
- repo 外路径

如果以后需要把 ignored 文件也纳入回滚范围，应该做显式开关，而不是默认全收。

## 7. 回滚语义

rollback 的目标不是“把 git branch 回退”，而是“把 workspace 回退到 checkpoint 对应的树状态”。

推荐算法：

1. 确认当前没有进行中的 turn。
2. 定位目标 checkpoint。
3. 检查当前 workspace 和 checkpoint 之间是否有用户后续改动。
4. 若有冲突，拒绝或要求显式确认。
5. 只恢复受 checkpoint 覆盖的路径。
6. 保持 `HEAD` 不动。

这意味着 rollback 的副作用只发生在 working tree，不发生在 branch history。

## 8. 冲突策略

rollback 前必须区分三类变化：

- agent 在 checkpoint 之后写入的文件
- 用户在 checkpoint 之后手动写入的文件
- 其他进程写入的文件

默认策略应该是保守的：

- 如果回滚会覆盖未被 checkpoint 覆盖的后续改动，拒绝。
- 如果只影响 agent 自己的改动，可以自动回滚。
- 如果路径范围不清晰，要求用户确认。

## 9. 与 session rewind 的关系

session rewind 和 workspace rollback 是两条不同的轴：

- **session rewind**：回滚对话历史、turn 状态、history 投影。
- **workspace rollback**：回滚文件系统改动。

它们可以共享同一个 `prompt_turn` 边界，但不要共享同一份数据模型。

建议做法是：

- storage 记录 `turn_seq` / `journal_seq`
- workspace checkpoint 记录同一个 `turn_seq`
- 这样之后可以把“回到第 N 个 turn”同时映射到 history 和 workspace

## 10. 实现建议

- 优先复用成熟实现，不要手写一套半成品 git 包装。
- Rust 侧建议优先考虑 `git2` 或 `gix` 这类库。
- 如果必须走命令行 plumbing，也应当只封装最小必要命令，不直接拼接危险的 shell 字符串。

## 11. 建议落地顺序

1. 先定义 checkpoint 元数据和 ref 命名。
2. 再实现 prompt turn 开始时的 checkpoint 创建。
3. 再实现只读 rollback 预览。
4. 最后补真正的 workspace restore。

## 12. 结论

`prompt_turn` 和 git checkpoint 适合挂钩，但应该挂到**隐藏 checkpoint refs**，不是用户分支 commit。这样既能提供 rollback 能力，又不破坏用户的 git 历史。
