import type { DshPlugin } from '../hooks/use-dsh-plugins'
import { CircleExclamation } from '@gravity-ui/icons'
import { Button, Description } from '@heroui/react'
import { useMutation } from '@tanstack/react-query'
import { invoke } from '@tauri-apps/api/core'
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { If } from 'react-if-lite'
import { useStore } from 'valtio-define'
import { store } from '@/store'
import { toast } from '@/utils'
import { useDshPlugins } from '../hooks/use-dsh-plugins'

/**
 * 已知坏版本插件的恢复入口（issue #44）。
 *
 * `@linxin666/dsh-client-ui-web-ui-settings` 等老版本在客户端 boot 阶段因
 * keyed slot 未带 `options.key` 整页崩溃，桌面端无法直接跳过/观察该崩溃，
 * 但可以识别「命中已知坏版本」的已安装插件，给出可操作的恢复动作：
 * - 升级到最新版（`dsh plugin --profile web add <id>`）
 * - 卸载（`dsh plugin --profile web remove <id>`）
 * 操作成功后触发服务重启以加载新 bundle，让其余插件正常加载、界面恢复。
 * 无命中坏的插件时不渲染任何内容。
 */
export default function PluginRecovery() {
  const { t } = useTranslation()
  const { plugins, refresh } = useDshPlugins()
  const harness = useStore(store.harness)
  // 记录正在操作中的插件 id，按钮进入 loading 态并禁用重复操作
  const [busyId, setBusyId] = useState<string | null>(null)

  const broken = plugins.filter(plugin => plugin.broken)

  const { mutate: runAction } = useMutation({
    mutationFn: async ({ plugin, action }: { plugin: DshPlugin, action: 'upgrade' | 'remove' }) => {
      setBusyId(plugin.id)
      if (action === 'upgrade')
        await invoke('upgrade_dsh_plugin', { id: plugin.id })
      else
        await invoke('remove_dsh_plugin', { id: plugin.id })
    },
    onSuccess: async () => {
      await refresh()
      toast(t('plugins.action_success'), {})
      // bundle 变更需重启服务才能生效，交由 harness 重启流程
      void harness.restart()
    },
    onError: (err) => {
      console.error('[PluginRecovery] plugin action failed:', err)
      toast(t('plugins.action_failed'), { variant: 'danger' })
    },
    onSettled: () => {
      setBusyId(null)
    },
  })

  if (broken.length === 0)
    return null

  return (
    <div className="rounded-md border border-warning/30 bg-warning/5 p-2 space-y-2">
      <div className="flex items-center gap-1.5 text-xs font-semibold text-warning">
        <CircleExclamation className="size-3.5" />
        <span>{t('plugins.broken_title')}</span>
      </div>
      {broken.map(plugin => (
        <div key={plugin.id} className="space-y-1">
          <Description className="text-[11px] leading-snug text-muted break-all">
            {t('plugins.broken_hint', { name: plugin.name, id: plugin.id, version: plugin.version })}
          </Description>
          <div className="flex gap-1.5">
            <If cond={busyId === plugin.id}>
              <span className="text-[11px] text-muted self-center">{t('plugins.restarting')}</span>
            </If>
            <Button
              size="sm"
              variant="primary"
              className="rounded-md h-7 text-xs"
              isDisabled={busyId !== null}
              onPress={() => runAction({ plugin, action: 'upgrade' })}
            >
              {t('plugins.upgrade')}
            </Button>
            <Button
              size="sm"
              variant="danger"
              className="rounded-md h-7 text-xs"
              isDisabled={busyId !== null}
              onPress={() => runAction({ plugin, action: 'remove' })}
            >
              {t('plugins.remove')}
            </Button>
          </div>
        </div>
      ))}
    </div>
  )
}
