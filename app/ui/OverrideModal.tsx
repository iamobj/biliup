'use client'

import {
  Form,
  Notification,
  Collapse,
  Select,
  Avatar,
  Button,
  Typography,
} from '@douyinfe/semi-ui'
import { FormApi } from '@douyinfe/semi-ui/lib/es/form'
import React, { useMemo, useRef, useState } from 'react'
import { LiveStreamerEntity, type MosaicConfig } from '../lib/api-streamer'
import { SupportedPlatforms } from '@/app/ui/plugins'
import { useBiliUsers } from '../lib/use-streamers'
import {
  applyChangedValuesToOverride,
  cloneOverride,
  compactOverrideRecord,
  formatOverrideText,
  parseOverrideText as parseSparseOverrideText,
  pickStreamerPayload,
  type OverrideRecord,
} from '@/app/lib/override-config'
import OverrideSwitch, { IsOverrideFormContext } from '@/app/ui/components/OverrideSwitch'
import { FileSizeField } from './FileSizeInput'
import { FormSheet } from './shell'
import { MosaicPanel } from './MosaicPanel'
import { SegmentTimeField } from './SegmentTimeField'
import { updateSegmentTimeOverride, validateSegmentTime, type SegmentTimeValue } from '../lib/segment-time'
import { parseOverrideText, updateMosaicOverrideText, validateMosaicConfig } from '../lib/mosaic-config'
import { applyLocalMaterialPreset } from '../lib/local-material-preset'

type PluginProps = {
  entity?: LiveStreamerEntity
  list?: { value: number; label: React.ReactNode }[]
  initValues?: OverrideRecord
}

type TemplateModalProps = {
  visible?: boolean
  entity?: LiveStreamerEntity
  children?: React.ReactNode
  initialPanel?: 'plugin' | 'mosaic'
  onOk: (e: any) => Promise<void>
  onCancel?: () => void
}

type LastEdited = 'form' | 'json'

type PlatformPattern = keyof typeof SupportedPlatforms

/** 按直播间地址找到对应平台插件在 SupportedPlatforms 里的键;没匹配到返回 undefined */
const matchPlatformPattern = (url?: string): PlatformPattern | undefined =>
  (Object.keys(SupportedPlatforms) as PlatformPattern[]).find(pattern => url?.match(new RegExp(pattern)))

const OverrideModal: React.FC<TemplateModalProps> = ({ children, entity, initialPanel = 'plugin', onOk, visible: controlledVisible, onCancel }) => {
  const [activePanels, setActivePanels] = useState<string[]>([initialPanel])
  const api = useRef<FormApi>(undefined)
  const lastEditedRef = useRef<LastEdited>('form')
  const overrideRef = useRef<OverrideRecord>({})
  const syncingRef = useRef(false)
  const [internalVisible, setInternalVisible] = useState(false)
  const visible = controlledVisible ?? internalVisible
  const setVisible = (next: boolean) => {
    setInternalVisible(next)
    if (!next) onCancel?.()
  }
  const [formKey, setFormKey] = useState(0)
  const localMaterialPresetApplied = useRef(false)

  const { biliUsers } = useBiliUsers(visible)
  const list = biliUsers?.map(item => {
    return {
      value: item.value,
      label: (
        <>
          <Avatar size="extra-small" src={item.face} />
          <span style={{ marginLeft: 8 }}>{item.name}</span>
        </>
      ),
    }
  })

  const initialOverride = useMemo(
    () => compactOverrideRecord(entity?.override as OverrideRecord | undefined),
    [entity?.id, visible]
  )

  const platformPlugin = useMemo(() => {
    const pattern = matchPlatformPattern(entity?.url)
    return pattern ? (SupportedPlatforms[pattern] as React.ComponentType<PluginProps>) : null
  }, [entity?.url])
  const showDialog = () => {
    setActivePanels([initialPanel])
    localMaterialPresetApplied.current = false
    const current = compactOverrideRecord(entity?.override as OverrideRecord | undefined)
    overrideRef.current = current
    lastEditedRef.current = 'form'
    setFormKey(prev => prev + 1)
    setVisible(true)
  }

  const applyOverrideToForm = (override: OverrideRecord, source: LastEdited) => {
    const formApi = api.current
    if (!formApi) return

    syncingRef.current = true
    lastEditedRef.current = source
    overrideRef.current = cloneOverride(override)

    const nextValues: Record<string, any> = {
      ...cloneOverride(override),
      override_text: formatOverrideText(override),
    }
    formApi.setValues(nextValues, { isOverride: true })
    queueMicrotask(() => {
      syncingRef.current = false
    })
  }

  const syncFormChangeToOverride = (changedValue: Record<string, any>) => {
    lastEditedRef.current = 'form'
    const next = applyChangedValuesToOverride(overrideRef.current, changedValue)
    overrideRef.current = next

    syncingRef.current = true
    api.current?.setValue('override_text', formatOverrideText(next))
    queueMicrotask(() => {
      syncingRef.current = false
    })
  }

  const syncJsonToForm = (jsonText?: string) => {
    const parsed = parseSparseOverrideText(jsonText)
    if (!parsed.ok) return
    applyOverrideToForm(parsed.value, 'json')
  }

  const syncMosaicConfig = (mosaicConfig?: MosaicConfig | null) => {
    const next = cloneOverride(overrideRef.current)
    if (mosaicConfig && mosaicConfig.enabled) {
      next.mosaic_config = mosaicConfig
    } else if (mosaicConfig && !mosaicConfig.enabled) {
      next.mosaic_config = { enabled: false, regions: [] }
    } else {
      delete next.mosaic_config
    }
    overrideRef.current = compactOverrideRecord(next)
    syncingRef.current = true
    api.current?.setValue('mosaic_config', next.mosaic_config)
    api.current?.setValue('override_text', formatOverrideText(overrideRef.current))
    queueMicrotask(() => {
      syncingRef.current = false
    })
  }

  const syncSegmentTime = (value: SegmentTimeValue) => {
    const next = cloneOverride(overrideRef.current)
    if (value === undefined) {
      delete next.segment_time
    } else {
      next.segment_time = value
    }
    overrideRef.current = next
    syncingRef.current = true
    api.current?.setValue('segment_time', value)
    api.current?.setValue('override_text', formatOverrideText(next))
    queueMicrotask(() => {
      syncingRef.current = false
    })
  }

  const applyLocalMaterialPresetToForm = () => {
    localMaterialPresetApplied.current = true
    const current = cloneOverride(overrideRef.current)
    const updated = applyLocalMaterialPreset(current)
    applyOverrideToForm(updated, 'form')
  }

  const handleCancel = () => {
    setVisible(false)
  }

  const handleOk = async () => {
    const submitted = await api.current?.validate()
    if (!submitted) return

    // 校验 JSON
    const jsonText = api.current?.getValue('override_text')
    if (jsonText && typeof jsonText === 'string') {
      const parsed = parseSparseOverrideText(jsonText)
      if (!parsed.ok) {
        Notification.error({
          title: '错误',
          content: '配置格式不正确，请检查 JSON 格式',
        })
        return
      }
    }

    const currentOverride = cloneOverride(overrideRef.current)
    const segmentTime = currentOverride.segment_time
    if (segmentTime !== undefined && segmentTime !== null) {
      const errorMsg = validateSegmentTime(segmentTime)
      if (errorMsg) {
        Notification.error({
          title: '视频分段时长无效',
          content: errorMsg,
        })
        return
      }
    }

    if (currentOverride.mosaic_config) {
      const errorMsg = validateMosaicConfig(currentOverride.mosaic_config)
      if (errorMsg) {
        Notification.error({
          title: '画面遮挡配置无效',
          content: errorMsg,
        })
        return
      }
    }

    const finalOverride = compactOverrideRecord(currentOverride)
    const payload: Record<string, any> = {
      ...pickStreamerPayload(entity),
      override: finalOverride,
    }

    if (localMaterialPresetApplied.current) {
      payload.postprocessor = []
    }

    try {
      await onOk(payload)
      setVisible(false)
    } catch (e: any) {
      Notification.error({
        title: '更新失败',
        content: e?.message ?? String(e),
      })
    }
  }

  const childrenWithProps = React.Children.map(children, child => {
    if (React.isValidElement(child)) {
      return React.cloneElement(child, {
        // @ts-ignore
        onClick: showDialog,
      })
    }
    return child
  })

  const downloadSettings = (
    <Collapse.Panel header="下载设置" itemKey="download">
      <div style={{ marginBottom: 12 }}>
        请到{' '}
        <a href="/dashboard" style={{ textDecoration: 'none', color: 'var(--semi-color-primary)' }}>
          空间配置
        </a>{' '}
        查看选项说明
      </div>
      <Form.Slot label="录制模式">
        <div>
          <Button theme="light" type="primary" onClick={applyLocalMaterialPresetToForm}>
            使用本地素材录制预设
          </Button>
          <div className="semi-form-field-extra">
            使用 mesio 落盘，不上传、不删除原始分段，并关闭录制阶段的画面遮挡；录后合成和切片在工作台完成。
          </div>
        </div>
      </Form.Slot>
      <Form.Select
        label="下载插件（downloader）"
        field="downloader"
        placeholder="mesio（默认）"
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      >
        <Select.Option value="streamlink">streamlink（hls多线程下载）</Select.Option>
        <Select.Option value="ffmpeg">ffmpeg</Select.Option>
        <Select.Option value="stream-gears">stream-gears</Select.Option>
        <Select.Option value="sync-downloader">sync-downloader（边录边传）</Select.Option>
        <Select.Option value="mesio">mesio（默认）</Select.Option>
      </Form.Select>

      <Form.Input field="uploader" initValue={entity?.override?.uploader} noLabel style={{ display: 'none' }} />

      <FileSizeField
        label="视频分段大小（file_size）"
        field="file_size"
        extraText="按 1024 进制：1 GB = 1024 MB。没填过的留空即跟随全局设置；把已有的值清空，则这个主播不按大小分段（边录边传仍约 2 GB 一段）。"
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
      />

      <SegmentTimeField
        field="segment_time"
        scope="room"
        initValue={initialOverride.segment_time}
        onChange={syncSegmentTime}
        label="视频分段时长（segment_time）"
        extraText="可选择预设或自定义时长。跟随全局、不按时长分段和独立时长分别保存；分段较短时请同时检查碎片过滤阈值，低于阈值的分段会被过滤。"
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
      />

      <Form.InputNumber
        field="filtering_threshold"
        label="碎片过滤（filtering_threshold）"
        suffix={'MB'}
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      />
    </Collapse.Panel>
  )

  const Plugin = platformPlugin

  return (
    <>
      {childrenWithProps}
      {visible && <FormSheet
        title={`${initialPanel === 'mosaic' ? '画面遮挡' : '配置覆写'}${entity?.remark ? `「${entity.remark}」` : ''}`}
        visible={visible}
        size="md"
        okText="保存"
        onOk={handleOk}
        onCancel={handleCancel}
      >
        {visible ? (
          <IsOverrideFormContext.Provider value={true}>
            <Form
              key={`${entity?.id ?? 'new'}-${formKey}`}
              initValues={{
                ...cloneOverride(initialOverride),
                override_text: formatOverrideText(initialOverride),
              }}
              getFormApi={formApi => {
                api.current = formApi
                overrideRef.current = compactOverrideRecord(initialOverride)
              }}
              onValueChange={(_values, changedValue) => {
                if (syncingRef.current) return
                const changedKeys = Object.keys(changedValue || {})
                if (changedKeys.length === 1 && changedKeys[0] === 'override_text') {
                  lastEditedRef.current = 'json'
                  return
                }
                const meaningful = Object.keys(changedValue || {}).filter(
                  key => key !== 'override_text'
                )
                if (!meaningful.length) return
                syncFormChangeToOverride(changedValue)
              }}
            >
              <div
                style={{
                  display: 'flex',
                  justifyContent: 'space-between',
                  alignItems: 'center',
                  marginBottom: 8,
                }}
              >
                <Typography.Text type="tertiary" size="small">
                  仅保存显式覆写项；清除字段后继承全局配置
                </Typography.Text>
                <Button
                  theme="borderless"
                  type="tertiary"
                  size="small"
                  onClick={() => applyOverrideToForm({}, 'form')}
                >
                  清空全部覆写
                </Button>
              </div>
              <Form.TextArea
                field="override_text"
                label="配置覆写"
                placeholder="请输入 JSON 格式的配置"
                style={{ marginBottom: 12 }}
                onBlur={e => {
                  const text = (e?.target as HTMLTextAreaElement | undefined)?.value
                    ?? api.current?.getValue('override_text')
                  const parsed = parseSparseOverrideText(text)
                  if (!parsed.ok) {
                    Notification.error({
                      title: '错误',
                      content: parsed.error,
                    })
                    return
                  }
                  syncJsonToForm(text)
                }}
                rules={[
                  { required: false },
                  {
                    validator: (_rule, value) => {
                      if (!value) return true
                      return parseSparseOverrideText(value).ok
                    },
                    message: '请输入有效的 JSON 格式',
                  },
                ]}
              />
              <Form.Section>
                <Collapse
                  activeKey={activePanels}
                  onChange={keys => setActivePanels(Array.isArray(keys) ? keys : keys ? [keys] : [])}
                  keepDOM
                  lazyRender={false}
                >
                  {downloadSettings}
                  <MosaicPanel
                    entity={entity}
                    active={visible && activePanels.includes('mosaic')}
                    initValues={initialOverride}
                    onChange={syncMosaicConfig}
                  />
                  {Plugin ? (
                    <Plugin entity={entity} list={list} initValues={initialOverride} />
                  ) : null}
                </Collapse>
              </Form.Section>
            </Form>
          </IsOverrideFormContext.Provider>
        ) : null}
      </FormSheet>}
    </>
  )
}

export default OverrideModal
