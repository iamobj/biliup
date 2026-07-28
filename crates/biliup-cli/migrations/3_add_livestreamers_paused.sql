-- 持久化用户主动暂停状态，应用重启后仍保持 Pause
ALTER TABLE livestreamers ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
