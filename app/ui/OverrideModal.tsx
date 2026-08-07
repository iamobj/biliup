'use client'
import {
  Form,
  Modal,
  Notification,
  Collapse,
  Select,
  Avatar,
  Button,
  Typography,
} from '@douyinfe/semi-ui'
import { FormApi } from '@douyinfe/semi-ui/lib/es/form'
import React, { useMemo, useRef, useState } from 'react'
import { LiveStreamerEntity } from '../lib/api-streamer'
import { SupportedPlatforms } from '@/app/ui/plugins'
import { useBiliUsers } from '../lib/use-streamers'
import {
  applyChangedValuesToOverride,
  cloneOverride,
  compactOverrideRecord,
  formatOverrideText,
  parseOverrideText,
  pickStreamerPayload,
  type OverrideRecord,
} from '@/app/lib/override-config'
import OverrideSwitch, { IsOverrideFormContext } from '@/app/ui/components/OverrideSwitch'

type PluginProps = {
  entity?: LiveStreamerEntity
  list?: { value: number; label: React.ReactNode }[]
  initValues?: OverrideRecord
}

type TemplateModalProps = {
  visible?: boolean
  entity?: LiveStreamerEntity
  children?: React.ReactNode
  onOk: (e: any) => Promise<void>
}

type LastEdited = 'form' | 'json'

const OverrideModal: React.FC<TemplateModalProps> = ({ children, entity, onOk }) => {
  const api = useRef<FormApi>()
  const lastEditedRef = useRef<LastEdited>('form')
  const overrideRef = useRef<OverrideRecord>({})
  const syncingRef = useRef(false)
  const [visible, setVisible] = useState(false)
  const [formKey, setFormKey] = useState(0)

  const { biliUsers } = useBiliUsers()
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
    // re-init only when opening a different entity or after external entity change while closed
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [entity?.id, visible]
  )

  const platformPlugin = useMemo(() => {
    if (!entity?.url) return null
    for (const [pattern, Plugin] of Object.entries(SupportedPlatforms)) {
      if (entity.url.match(new RegExp(pattern))) {
        return Plugin as React.ComponentType<PluginProps>
      }
    }
    return null
  }, [entity?.url])

  const showDialog = () => {
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
    // allow form fields to settle before accepting user edits again
    queueMicrotask(() => {
      syncingRef.current = false
    })
  }

  const writeOverrideText = (override: OverrideRecord) => {
    const text = formatOverrideText(override)
    const currentText = api.current?.getValue('override_text')
    if (currentText === text) return
    syncingRef.current = true
    api.current?.setValue('override_text', text)
    queueMicrotask(() => {
      syncingRef.current = false
    })
  }

  // Only merge explicitly changed form paths into override.
  // Untouched controls never enter JSON.
  const syncFormChangeToOverride = (changedValue?: Record<string, any>) => {
    if (syncingRef.current) return
    const override = applyChangedValuesToOverride(overrideRef.current, changedValue)
    overrideRef.current = override
    lastEditedRef.current = 'form'
    writeOverrideText(override)
  }

  const syncJsonToForm = (text?: string) => {
    if (syncingRef.current) return
    const parsed = parseOverrideText(text)
    if (!parsed.ok) {
      return false
    }
    applyOverrideToForm(parsed.value, 'json')
    return true
  }

  const handleOk = async () => {
    try {
      await api.current?.validate()
    } catch {
      return
    }

    let override: OverrideRecord
    if (lastEditedRef.current === 'json') {
      const text = api.current?.getValue('override_text')
      const parsed = parseOverrideText(text)
      if (!parsed.ok) {
        Notification.error({
          title: '错误',
          content: parsed.error,
        })
        return
      }
      override = compactOverrideRecord(parsed.value)
    } else {
      // form edits already merged into overrideRef via onValueChange
      override = compactOverrideRecord(overrideRef.current)
    }

    const payload = {
      ...pickStreamerPayload(entity as Record<string, any>),
      override,
    }

    await onOk(payload)
    setVisible(false)
  }

  const handleCancel = () => {
    setVisible(false)
  }

  const childrenWithProps = React.Children.map(children, child => {
    if (React.isValidElement<any>(child)) {
      return React.cloneElement(child, {
        onClick: () => {
          showDialog()
          child.props.onClick?.()
        },
      })
    }
  })

  const downloadSettings = (
    <Collapse.Panel header="下载设置" itemKey="download">
      <div style={{ marginBottom: 12 }}>
        请到
        <a href="/dashboard" style={{ textDecoration: 'none', color: 'var(--semi-color-primary)' }}>
          空间配置
        </a>
        查看选项说明
      </div>
      <Form.Select
        label="下载插件（downloader）"
        field="downloader"
        placeholder="stream-gears（默认）"
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      >
        <Select.Option value="streamlink">streamlink（hls多线程下载）</Select.Option>
        <Select.Option value="ffmpeg">ffmpeg</Select.Option>
        <Select.Option value="stream-gears">stream-gears（默认）</Select.Option>
        <Select.Option value="sync-downloader">sync-downloader（边录边传）</Select.Option>
      </Form.Select>

      <Form.InputNumber
        label="视频分段大小（file_size）"
        field="file_size"
        placeholder=""
        suffix={'Byte'}
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      />

      <Form.Input
        field="segment_time"
        label="视频分段时长（segment_time）"
        placeholder="01:00:00"
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
        rules={[
          {
            pattern: /^[^：]*$/,
            message: '请使用英文冒号',
          },
          {
            pattern: /^[0-9:]*$/,
            message: '只接受数字和英文冒号',
          },
          {
            pattern: /^$|^[0-9]{2,4}:[0-5][0-9]:[0-5][0-9]$/,
            message: '分或秒不符合规范',
          },
        ]}
        stopValidateWithError={true}
      />

      <OverrideSwitch
        field="split_on_timestamp_anomaly"
        label="时间戳异常自动切文件（split_on_timestamp_anomaly）"
        extraText={
          <div style={{ fontSize: '14px' }}>
            检测到直播流时间戳回退/非单调时自动切文件；单调前跳不切。未覆写时继承全局配置（默认开启）。
          </div>
        }
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
      <Modal
        title="配置覆写"
        visible={visible}
        onOk={handleOk}
        style={{ width: 'min(600px, 90vw)' }}
        onCancel={handleCancel}
        bodyStyle={{
          overflow: 'auto',
          maxHeight: 'calc(100vh - 320px)',
          paddingLeft: 10,
          paddingRight: 10,
        }}
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
              // Ignore bulk setValues payloads that only refresh override_text
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
                const parsed = parseOverrideText(text)
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
                    return parseOverrideText(value).ok
                  },
                  message: '请输入有效的 JSON 格式',
                },
              ]}
            />
            <Form.Section>
              <Collapse defaultActiveKey={['plugin', 'download']}>
                {downloadSettings}
                {Plugin ? (
                  <Plugin entity={entity} list={list} initValues={initialOverride} />
                ) : null}
              </Collapse>
            </Form.Section>
          </Form>
          </IsOverrideFormContext.Provider>
        ) : null}
      </Modal>
    </>
  )
}

export default OverrideModal
