-- 将历史布尔配置 split_on_timestamp_anomaly 迁移为数值毫秒 timestamp_anomaly_threshold_ms
-- true/null -> 5000, false -> 0；并删除旧键

-- 1. 迁移 configuration 表 (key = 'config')
UPDATE configuration
SET value = json_set(
    json_remove(value, '$.split_on_timestamp_anomaly'),
    '$.timestamp_anomaly_threshold_ms',
    CASE
        WHEN json_extract(value, '$.split_on_timestamp_anomaly') = 0
             OR json_extract(value, '$.split_on_timestamp_anomaly') = false THEN 0
        ELSE 5000
    END
)
WHERE key = 'config'
  AND json_type(value, '$.split_on_timestamp_anomaly') IS NOT NULL;

-- 2. 迁移 livestreamers 表 (override 字段)
UPDATE livestreamers
SET override = json_set(
    json_remove(override, '$.split_on_timestamp_anomaly'),
    '$.timestamp_anomaly_threshold_ms',
    CASE
        WHEN json_extract(override, '$.split_on_timestamp_anomaly') = 0
             OR json_extract(override, '$.split_on_timestamp_anomaly') = false THEN 0
        ELSE 5000
    END
)
WHERE override IS NOT NULL
  AND json_type(override, '$.split_on_timestamp_anomaly') IS NOT NULL;
