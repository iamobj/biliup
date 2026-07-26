export const STREAMER_ENTITY_FIELDS = new Set([
  'id',
  'url',
  'remark',
  'filename_prefix',
  'time_range',
  'upload_streamers_id',
  'format',
  'excluded_keywords',
  'preprocessor',
  'segment_processor',
  'downloaded_processor',
  'postprocessor',
  'opt_args',
  'override',
])

export const STREAMER_UI_ONLY_FIELDS = new Set([
  'status',
  'upload_status',
  'statusTag',
  'override_text',
])

export type OverrideRecord = Record<string, any>

export function cloneOverride(override?: OverrideRecord | null): OverrideRecord {
  if (!override || typeof override !== 'object' || Array.isArray(override)) {
    return {}
  }
  return structuredClone(override)
}

/** Keys where explicit null is meaningful and should be kept. */
const NULLABLE_OVERRIDE_KEYS = new Set(['file_size'])

/**
 * Drop noise nulls produced by historical dense ConfigPatch serialization.
 * Keep only explicit override values (and nullable clears like file_size: null).
 */
export function compactOverrideRecord(override?: OverrideRecord | null): OverrideRecord {
  if (!override || typeof override !== 'object' || Array.isArray(override)) {
    return {}
  }
  const result: OverrideRecord = {}
  Object.entries(override).forEach(([key, value]) => {
    if (value === null) {
      if (NULLABLE_OVERRIDE_KEYS.has(key)) {
        result[key] = null
      }
      return
    }
    if (isPlainObject(value)) {
      const nested = compactOverrideRecord(value)
      if (Object.keys(nested).length > 0) {
        result[key] = nested
      }
      return
    }
    if (value !== undefined) {
      result[key] = value
    }
  })
  return result
}


export function isPlainObject(value: unknown): value is Record<string, any> {
  return !!value && typeof value === 'object' && !Array.isArray(value)
}

export function setPathValue(target: OverrideRecord, path: string, value: any) {
  const parts = path.split('.').filter(Boolean)
  if (!parts.length) return

  let cursor: Record<string, any> = target
  for (let i = 0; i < parts.length - 1; i++) {
    const key = parts[i]
    if (!isPlainObject(cursor[key])) {
      cursor[key] = {}
    }
    cursor = cursor[key]
  }
  cursor[parts[parts.length - 1]] = value
}

export function deletePathValue(target: OverrideRecord, path: string) {
  const parts = path.split('.').filter(Boolean)
  if (!parts.length) return

  const stack: Array<{ parent: Record<string, any>; key: string }> = []
  let cursor: Record<string, any> = target
  for (let i = 0; i < parts.length - 1; i++) {
    const key = parts[i]
    if (!isPlainObject(cursor[key])) {
      return
    }
    stack.push({ parent: cursor, key })
    cursor = cursor[key]
  }

  delete cursor[parts[parts.length - 1]]

  for (let i = stack.length - 1; i >= 0; i--) {
    const { parent, key } = stack[i]
    const child = parent[key]
    if (isPlainObject(child) && Object.keys(child).length === 0) {
      delete parent[key]
    } else {
      break
    }
  }
}

export function getPathValue(source: OverrideRecord | undefined | null, path: string): any {
  if (!source) return undefined
  const parts = path.split('.').filter(Boolean)
  let cursor: any = source
  for (const part of parts) {
    if (!isPlainObject(cursor) || !(part in cursor)) {
      return undefined
    }
    cursor = cursor[part]
  }
  return cursor
}

export function hasPathValue(source: OverrideRecord | undefined | null, path: string): boolean {
  if (!source) return false
  const parts = path.split('.').filter(Boolean)
  let cursor: any = source
  for (const part of parts) {
    if (!isPlainObject(cursor) || !(part in cursor)) {
      return false
    }
    cursor = cursor[part]
  }
  return true
}

export function normalizeOverrideValue(value: any): any {
  if (value === '') return null
  return value
}

function isIgnoredFormPath(path: string): boolean {
  const root = path.split('.')[0]
  return STREAMER_ENTITY_FIELDS.has(root) || STREAMER_UI_ONLY_FIELDS.has(root)
}

/**
 * Apply Semi Form changedValues onto an explicit override object.
 * - undefined => delete key (inherit global)
 * - '' => null (explicit clear for optional string/number-like fields)
 * - false/0/null/other => keep as explicit override
 * Only touched paths are written; untouched form defaults never appear.
 */
export function applyChangedValuesToOverride(
  current: OverrideRecord | undefined | null,
  changedValue?: Record<string, any> | null
): OverrideRecord {
  const result = cloneOverride(current)
  if (!changedValue) return result

  Object.entries(changedValue).forEach(([path, value]) => {
    if (!path || isIgnoredFormPath(path)) return

    if (value === undefined) {
      deletePathValue(result, path)
      return
    }

    // Rare: Semi may pass a nested object for a parent path.
    if (isPlainObject(value)) {
      const nested = normalizeNestedObject(value)
      if (Object.keys(nested).length === 0) {
        deletePathValue(result, path)
      } else {
        setPathValue(result, path, nested)
      }
      return
    }

    setPathValue(result, path, normalizeOverrideValue(value))
  })

  return result
}

function normalizeNestedObject(value: Record<string, any>): OverrideRecord {
  const result: OverrideRecord = {}
  Object.entries(value).forEach(([key, child]) => {
    if (child === undefined) return
    if (isPlainObject(child)) {
      const nested = normalizeNestedObject(child)
      if (Object.keys(nested).length > 0) {
        result[key] = nested
      }
      return
    }
    result[key] = normalizeOverrideValue(child)
  })
  return result
}

/** @deprecated prefer applyChangedValuesToOverride for explicit-only semantics */
export function buildOverrideFromFormValues(
  values: Record<string, any> | undefined | null,
  options?: {
    entityFields?: Set<string>
    uiOnlyFields?: Set<string>
  }
): OverrideRecord {
  const entityFields = options?.entityFields ?? STREAMER_ENTITY_FIELDS
  const uiOnlyFields = options?.uiOnlyFields ?? STREAMER_UI_ONLY_FIELDS
  const result: OverrideRecord = {}
  if (!values) return result

  Object.entries(values).forEach(([key, value]) => {
    if (entityFields.has(key) || uiOnlyFields.has(key)) return
    if (value === undefined) return

    if (isPlainObject(value)) {
      const nested = normalizeNestedObject(value)
      if (Object.keys(nested).length > 0) {
        result[key] = nested
      }
      return
    }

    result[key] = normalizeOverrideValue(value)
  })

  return result
}

export function pickStreamerPayload(entity?: Record<string, any> | null): Record<string, any> {
  if (!entity) return {}
  const payload: Record<string, any> = {}
  STREAMER_ENTITY_FIELDS.forEach(field => {
    if (field === 'override') return
    if (Object.prototype.hasOwnProperty.call(entity, field)) {
      payload[field] = entity[field]
    }
  })
  return payload
}

export function formatOverrideText(override?: OverrideRecord | null): string {
  const value = compactOverrideRecord(override)
  if (Object.keys(value).length === 0) return ''
  return JSON.stringify(value, null, 2)
}

export function parseOverrideText(text: string | undefined | null): {
  ok: true
  value: OverrideRecord
} | {
  ok: false
  error: string
} {
  if (text == null || text.trim() === '') {
    return { ok: true, value: {} }
  }
  try {
    const parsed = JSON.parse(text)
    if (!isPlainObject(parsed)) {
      return { ok: false, error: '配置覆写必须是 JSON 对象' }
    }
    return { ok: true, value: compactOverrideRecord(parsed) }
  } catch {
    return { ok: false, error: '请输入有效的 JSON 格式' }
  }
}
