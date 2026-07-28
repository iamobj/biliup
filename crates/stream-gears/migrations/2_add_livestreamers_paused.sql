-- 与 biliup-cli 迁移保持 schema 对齐：持久化用户主动暂停状态
ALTER TABLE livestreamers ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
