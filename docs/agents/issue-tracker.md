# Issue tracker：本地 Markdown

本仓库的议题（issue）与规格（spec）以 markdown 文件形式存放在 `.scratch/` 目录中，不使用 GitHub Issues 等远程跟踪服务。

## 约定

- 每个功能一个目录：`.scratch/<feature-slug>/`
- 规格文件为 `.scratch/<feature-slug>/spec.md`
- 实施工题一票一文件，位于 `.scratch/<feature-slug>/issues/<NN>-<slug>.md`，从 `01` 起编号；不使用把所有票塞进一个文件的做法
- 三态状态记录在每个议题文件顶部附近的 `Status:` 行，取值见 `triage-labels.md` 中的角色字符串
- 评论与讨论历史追加到文件底部 `## Comments` 节之下

## 当技能说「发布到 issue tracker」

在 `.scratch/<feature-slug>/` 下新建文件（目录不存在则先创建）。

## 当技能说「获取相关工票」

读取所引用路径的文件。用户通常会直接给出路径或议题编号。

## Wayfinding 操作

供 `/wayfinder` 使用。**地图**是一个文件，每张**子票**对应一个子文件。

- **地图**：`.scratch/<effort>/map.md`（Notes / Decisions-so-far / Fog 正文）。
- **子票**：`.scratch/<effort>/issues/NN-<slug>.md`，从 `01` 起编号，正文写明问题。`Type:` 行记录票型（`research` / `prototype` / `grilling` / `task`）；`Status:` 行记录 `claimed` / `resolved`。
- **阻塞**：文件顶部附近一行 `Blocked by: NN, NN`。所列文件全部 `resolved` 后该票解除阻塞。
- **前沿**：扫描 `.scratch/<effort>/issues/` 中开放、无阻塞、未被认领的文件；编号最小者优先。
- **认领**：开工前先把 `Status:` 改为 `claimed` 并保存。
- **解决**：在 `## Answer` 节下追加答案，`Status:` 改为 `resolved`，并在 `map.md` 的 Decisions-so-far 追加上下文指针（要点 + 链接）。
