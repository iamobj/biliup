# biliup 二开版本

本仓库基于上游项目 [biliup/biliup](https://github.com/biliup/biliup) 进行二次开发，主要用于维护本分支需要的录制、分段处理和 Hook 行为调整。

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
