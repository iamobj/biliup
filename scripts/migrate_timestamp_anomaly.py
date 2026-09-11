#!/usr/bin/env python3
"""
Migrate legacy `split_on_timestamp_anomaly` (boolean) to `timestamp_anomaly_threshold_ms` (integer milliseconds).
- True  -> 5000
- False -> 0
- None  -> 5000
The legacy `split_on_timestamp_anomaly` key is deleted after migration.
"""

import json
import os
import sqlite3
import sys
from pathlib import Path
from typing import Optional


def find_default_db_path() -> Optional[Path]:
    candidates = [
        Path("data.sqlite3"),
        Path("data/data.sqlite3"),
        Path("crates/biliup-cli/data/data.sqlite3"),
        Path("crates/biliup-cli/data.sqlite3"),
    ]
    for c in candidates:
        if c.is_file():
            return c
    return None


def migrate_value(val) -> int:
    if val is True or val is None:
        return 5000
    if val is False:
        return 0
    if isinstance(val, (int, float)):
        return int(val)
    return 5000


def migrate_database(db_path: Path):
    print(f"Connecting to database: {db_path.resolve()}")
    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()

    # 1. Migrate `configuration` table (key = 'config')
    cursor.execute("SELECT id, value FROM configuration WHERE key = 'config'")
    row = cursor.fetchone()
    if row:
        cfg_id, cfg_val = row
        try:
            cfg_json = json.loads(cfg_val)
            modified = False
            if "split_on_timestamp_anomaly" in cfg_json:
                old_val = cfg_json.pop("split_on_timestamp_anomaly")
                new_val = migrate_value(old_val)
                cfg_json["timestamp_anomaly_threshold_ms"] = new_val
                modified = True
                print(f"[configuration] Migrated split_on_timestamp_anomaly={old_val} -> timestamp_anomaly_threshold_ms={new_val}")
            elif "timestamp_anomaly_threshold_ms" not in cfg_json:
                cfg_json["timestamp_anomaly_threshold_ms"] = 5000
                modified = True
                print("[configuration] Set default timestamp_anomaly_threshold_ms=5000")

            if modified:
                new_str = json.dumps(cfg_json, ensure_ascii=False)
                cursor.execute("UPDATE configuration SET value = ? WHERE id = ?", (new_str, cfg_id))
                print("[configuration] Updated successfully.")
        except Exception as e:
            print(f"[configuration] Error parsing/updating config JSON: {e}", file=sys.stderr)

    # 2. Migrate `livestreamers` table (override column)
    cursor.execute("SELECT id, remark, override FROM livestreamers WHERE override IS NOT NULL")
    streamers = cursor.fetchall()
    streamers_migrated = 0
    for s_id, remark, s_override in streamers:
        if not s_override:
            continue
        try:
            ov_json = json.loads(s_override)
            if isinstance(ov_json, dict) and "split_on_timestamp_anomaly" in ov_json:
                old_val = ov_json.pop("split_on_timestamp_anomaly")
                new_val = migrate_value(old_val)
                ov_json["timestamp_anomaly_threshold_ms"] = new_val
                new_str = json.dumps(ov_json, ensure_ascii=False)
                cursor.execute("UPDATE livestreamers SET override = ? WHERE id = ?", (new_str, s_id))
                streamers_migrated += 1
                print(f"[livestreamer {s_id}: {remark}] Migrated override: {old_val} -> {new_val}")
        except Exception as e:
            print(f"[livestreamer {s_id}] Error parsing override JSON: {e}", file=sys.stderr)

    conn.commit()
    conn.close()
    print(f"Migration completed! Migrated configuration and {streamers_migrated} livestreamers.")


def main():
    if len(sys.argv) > 1:
        db_path = Path(sys.argv[1])
    else:
        db_path = find_default_db_path()

    if not db_path or not db_path.exists():
        print("Usage: python3 migrate_timestamp_anomaly.py <path_to_data.sqlite3>", file=sys.stderr)
        print("Could not automatically locate database file.", file=sys.stderr)
        sys.exit(1)

    migrate_database(db_path)


if __name__ == "__main__":
    main()
