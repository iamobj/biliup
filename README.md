# biliup 二开版本

本仓库基于上游项目 [biliup/biliup](https://github.com/biliup/biliup) 进行二次开发，主要用于维护本分支需要的录制、分段处理和 Hook 行为调整。

## 二开功能说明

当前分支相对上游重点维护以下行为：

- `preprocessor` Hook 在开始下载直播前执行时，会通过标准输入传入 JSON 数据：
  - `name`：主播名称
  - `url`：开播地址
  - `start_time`：开播时间，Unix 秒级时间戳
- `segment_processor` Hook 在没有配置投稿模板时仍会执行。
  - 每个分段事件都会先生成视频/弹幕路径列表。
  - 如配置了 `segment_processor`，会在无上传流程下照常执行。
  - 单个分段处理失败时只跳过该分段，不中断后续分段处理。
  - 成功处理后的路径会继续交给 `postprocessor`。

这些调整主要面向只录制、不投稿，或需要用 Hook 接管分段后处理的场景。

## 与上游同步

本仓库 remote 约定：

- `origin`：二开仓库，当前为 `git@github.com:iamobj/biliup.git`
- `upstream`：上游源仓库，当前为 `https://github.com/biliup/biliup.git`

同步上游前建议先确认工作区干净，避免把本地未完成修改混入同步提交：

```bash
git status
```

拉取上游更新：

```bash
git fetch upstream
```

将上游默认分支合入当前二开分支：

```bash
git merge upstream/master
```

如上游默认分支改为 `main`，使用：

```bash
git merge upstream/main
```

解决冲突后继续提交：

```bash
git status
git add <conflict-files>
git commit
```

同步完成后推送到二开仓库：

```bash
git push origin HEAD
```

## 同步注意事项

- 优先保留本仓库的 Hook 行为调整，尤其是 `preprocessor` 输入 JSON 和无投稿模板时执行 `segment_processor`。
- 上游如改动下载、上传、Hook、配置导入相关代码，同步后需要重点检查：
  - `crates/biliup-cli/src/server/common/download.rs`
  - `crates/biliup-cli/src/server/common/upload.rs`
  - `crates/biliup-cli/src/server/infrastructure/models/hook_step.rs`

