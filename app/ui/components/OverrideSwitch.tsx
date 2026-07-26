'use client'
import React, { createContext, useContext } from 'react'
import { Button, Form, Typography, useFormApi, useFormState } from '@douyinfe/semi-ui'
import { hasPathValue } from '@/app/lib/override-config'

export const IsOverrideFormContext = createContext(false)

type Props = {
  field: string
  label: React.ReactNode
  extraText?: React.ReactNode
  fieldStyle?: React.CSSProperties
}

/**
 * Boolean control shared by dashboard and override modal.
 * - dashboard: plain switch
 * - override modal: three-state unset | true | false, with clear action
 */
const OverrideSwitch: React.FC<Props> = ({ field, label, extraText, fieldStyle }) => {
  const isOverrideForm = useContext(IsOverrideFormContext)
  const formApi = useFormApi()
  const formState = useFormState()
  const values = (formState?.values || {}) as Record<string, any>
  const overridden = isOverrideForm && hasPathValue(values, field)
  const style =
    fieldStyle ||
    ({
      alignSelf: 'stretch',
      padding: 0,
    } as React.CSSProperties)

  if (!isOverrideForm) {
    return <Form.Switch field={field} label={label} extraText={extraText} fieldStyle={style} />
  }

  return (
    <div style={{ marginBottom: 12 }}>
      <Form.Switch field={field} label={label} extraText={extraText} fieldStyle={style} />
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          marginTop: -4,
          marginBottom: 4,
        }}
      >
        <Typography.Text type="tertiary" size="small">
          {overridden ? '已覆写，清除后继承全局配置' : '未覆写，继承全局配置'}
        </Typography.Text>
        {overridden ? (
          <Button
            theme="borderless"
            type="tertiary"
            size="small"
            onClick={() => {
              formApi.setValue(field, undefined)
            }}
          >
            清除覆写
          </Button>
        ) : null}
      </div>
    </div>
  )
}

export default OverrideSwitch
