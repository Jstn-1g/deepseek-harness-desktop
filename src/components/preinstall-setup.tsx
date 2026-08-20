import type { PreinstallPlugin } from '../store/modules/harness'
import type { PreinstallPluginStatus } from '../store/modules/harness/types'
import { CircleCheck, CircleInfo, CircleXmark, Copy, Xmark } from '@gravity-ui/icons'
import { Button, Card, Checkbox, Chip } from '@heroui/react'
import { invoke } from '@tauri-apps/api/core'
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { If } from 'react-if-lite'
import { useStore } from 'valtio-define'
import { harness } from '../store/modules/harness'
import { toast } from '../utils/toast'

/**
 * 预装插件引导页：首次安装（或老版本升级）后展示推荐插件列表，
 * 用户确认后调用 `dsh plugin` 安装（日志实时回流到控制台），
 * 或跳过；两者都会标记完成并继续启动服务。
 */

/** 插件列表的一行：勾选框 + 名称 + 推荐/已安装标签 + 仓库跳转按钮 */
function PluginStatus({ status }: { status: PreinstallPluginStatus }) {
  if (status === 'installing') {
    return <span className="h-4 w-4 animate-load-spin rounded-full border-2 border-load-ring border-t-load-ink" />
  }
  if (status === 'success') {
    return <CircleCheck className="size-4 text-success" />
  }
  return <CircleXmark className="size-4 text-danger" />
}

function PluginRow({ plugin, checked, disabled, status, onToggle, onOpenRepo }: {
  plugin: PreinstallPlugin
  checked: boolean
  disabled: boolean
  status?: PreinstallPluginStatus
  onToggle: (id: string, checked: boolean) => void
  onOpenRepo: (id: string) => void
}) {
  const { t } = useTranslation()

  return (
    <div className="flex items-center justify-between gap-3 border-b border-line/40 px-4 py-3 last:border-b-0">
      <Checkbox
        isSelected={checked || plugin.installed}
        isDisabled={disabled || plugin.installed}
        onChange={isSelected => onToggle(plugin.id, isSelected)}
        className="min-w-0 flex-1"
      >
        <Checkbox.Content className="min-w-0">
          <Checkbox.Control className="bg-panel2 rounded-md">
            <Checkbox.Indicator className="rounded-md" />
          </Checkbox.Control>
          <span className="min-w-0">
            <span className={`flex min-w-0 items-center gap-2 text-sm font-medium ${plugin.installed ? 'text-muted line-through' : 'text-ink'}`}>
              <span className="truncate">{plugin.name}</span>
              <If cond={plugin.recommended && !plugin.installed}>
                <Chip size="sm" variant="soft" color="success" className="font-medium">
                  {t('preinstall.recommend')}
                </Chip>
              </If>
              <If cond={plugin.fix && !plugin.installed}>
                <Chip size="sm" variant="soft" color="warning" className="font-medium">
                  {t('preinstall.fix')}
                </Chip>
              </If>
              <If cond={plugin.installed}>
                <Chip size="sm" variant="soft" color="success" className="font-medium">
                  {t('preinstall.installed')}
                </Chip>
              </If>
            </span>
          </span>
        </Checkbox.Content>
      </Checkbox>
      <If cond={status !== undefined}>
        <PluginStatus status={status!} />
      </If>
      <Button
        isIconOnly
        size="sm"
        variant="ghost"
        className="shrink-0 rounded-md"
        aria-label={t('preinstall.open_repo', { name: plugin.name })}
        onPress={() => onOpenRepo(plugin.id)}
      >
        <CircleInfo className="size-4" />
      </Button>
    </div>
  )
}

/** 日志控制台：dsh plugin 进程输出，顶部带复制按钮，样式与安装/加载页日志面板一致 */
function LogPanel({ logs }: { logs: readonly string[] }) {
  const { t } = useTranslation()
  const text = logs.join('\n')

  async function copyLogs() {
    try {
      await navigator.clipboard.writeText(text || '')
      toast(t('messages.log_copied'), {})
    }
    catch (err) {
      console.error('[Harness] copy preinstall logs failed:', err)
    }
  }

  return (
    <div className="overflow-hidden rounded-lg border border-line bg-log-bg">
      {/* 面板头：复制日志（右上角） */}
      <div className="flex items-center justify-end border-b border-line/40 bg-panel2/60 px-2 py-1">
        <Button
          size="sm"
          variant="ghost"
          isIconOnly
          className="size-6 min-w-6 rounded-md"
          aria-label={t('buttons.copy')}
          onPress={copyLogs}
        >
          <Copy className="size-3.5" />
        </Button>
      </div>
      <div
        className="max-h-[240px] min-h-[112px] overflow-y-auto px-3.5 py-2.5 text-left font-mono text-xs leading-[1.7]"
        aria-label={t('ui.install_log')}
      >
        <If cond={logs.length > 0} else={<p className="m-0 text-load-muted">{t('ui.waiting_logs')}</p>}>
          {logs.slice(-100).map((line, index) => (
            // 日志行内容可能重复，以 index 区分 key
            // eslint-disable-next-line react/no-array-index-key
            <p key={`${line}-${index}`} className="m-0 flex gap-2 overflow-hidden text-ellipsis whitespace-nowrap text-log-ink">
              <span className="shrink-0 text-accent select-none">›</span>
              <span className="min-w-0 overflow-hidden text-ellipsis">{line}</span>
            </p>
          ))}
        </If>
      </div>
    </div>
  )
}

export default function PreinstallSetup() {
  const { t } = useTranslation()
  const { preinstall } = useStore(harness)
  // 用户手动调整后的选择（一旦交互即接管默认勾选）
  const [selected, setSelected] = useState<Set<string>>(() => new Set())
  const [touched, setTouched] = useState(false)

  // 进入引导页时拉取插件列表
  useEffect(() => {
    void harness.loadPreinstallPlugins()
  }, [])

  // 默认勾选：未安装的推荐插件 +「修复」类项 + 无 chip 但标记默认勾选的项（如 dsh-notification）。
  // 派生计算而非在加载回调里 setState，避免与 store 的加载去重守卫竞争，
  // 保证插件到位后默认勾选必定生效（用户手动调整后以用户选择为准）。
  const effectiveSelected = !touched
    ? new Set(preinstall.plugins.filter(p => !p.installed && (p.recommended || p.fix || p.defaultChecked)).map(p => p.id))
    : selected

  function toggle(id: string, checked: boolean) {
    setTouched(true)
    setSelected((prev) => {
      const next = new Set(prev)
      if (checked) {
        next.add(id)
      }
      else {
        next.delete(id)
      }
      return next
    })
  }

  function openRepo(id: string) {
    void invoke('open_preinstall_repo', { id }).catch((err) => {
      console.error('[Harness] open preinstall repo failed:', err)
    })
  }

  function handleConfirm() {
    void harness.confirmPreinstall([...effectiveSelected])
  }

  function handleSkip() {
    void harness.skipPreinstall()
  }

  function handleRetryFailed() {
    void harness.confirmPreinstall(Object.keys(preinstall.failed))
  }

  // 可选中的插件（未安装项）勾选数，用于禁用"确定"
  const selectableCount = preinstall.plugins.filter(p => !p.installed).length
  const selectedCount = [...effectiveSelected].filter(id => preinstall.plugins.some(p => p.id === id && !p.installed)).length
  const failedIds = Object.keys(preinstall.failed)
  const failedPlugins = preinstall.plugins.filter(plugin => failedIds.includes(plugin.id))
  const installing = preinstall.installing

  return (
    <div className="flex h-full w-full items-center justify-center bg-canvas">
      <div className="flex w-[min(560px,88vw)] flex-col gap-5">
        <header className="flex flex-col items-center gap-1.5 text-center">
          <h1 className="text-base font-semibold tracking-[0.08em] text-load-ink">{t('preinstall.title')}</h1>
          <p className="max-w-[440px] text-xs leading-5 text-load-muted">{t('preinstall.subtitle')}</p>
        </header>

        <If
          cond={installing}
          then={(
            <div className="flex flex-col gap-2.5">
              <Card className="p-0 rounded-md">
                {preinstall.plugins.map(plugin => (
                  <PluginRow
                    key={plugin.id}
                    plugin={plugin}
                    checked={effectiveSelected.has(plugin.id)}
                    status={preinstall.status[plugin.id]}
                    disabled
                    onToggle={toggle}
                    onOpenRepo={openRepo}
                  />
                ))}
              </Card>
              <p className="text-center text-xs leading-[18px] text-load-muted">{t('preinstall.installing')}</p>
              <LogPanel logs={preinstall.logs} />
              <div className="flex items-center justify-center">
                <Button className="rounded-md" size="sm" variant="tertiary" onPress={harness.cancelPreinstall} isDisabled={preinstall.cancelling}>
                  <Xmark className="size-3.5" />
                  {preinstall.cancelling ? t('preinstall.cancelling') : t('preinstall.cancel')}
                </Button>
              </div>
            </div>
          )}
          else={(
            <If
              cond={preinstall.error !== ''}
              then={(
                <div className="flex flex-col gap-2.5">
                  <div className="flex flex-col gap-2 rounded-lg border border-danger/30 bg-danger/5 px-3.5 py-3">
                    <p className="text-xs font-medium text-danger">{t('preinstall.failed')}</p>
                    <p className="max-h-[120px] overflow-y-auto break-all font-mono text-[11px] leading-relaxed text-load-muted">{preinstall.error}</p>
                  </div>
                  <LogPanel logs={preinstall.logs} />
                  <div className="flex items-center justify-end gap-2">
                    <Button className="rounded-md" size="sm" variant="tertiary" onPress={handleSkip}>{t('preinstall.skip')}</Button>
                    <Button className="rounded-md" size="sm" variant="primary" onPress={handleConfirm} isDisabled={selectedCount === 0 || selectableCount === 0}>{t('app.retry')}</Button>
                  </div>
                </div>
              )}
              else={(
                <If
                  cond={failedIds.length > 0}
                  then={(
                    <div className="flex flex-col gap-2.5">
                      <div className="flex flex-col gap-2 rounded-lg border border-warning/30 bg-warning/5 px-3.5 py-3">
                        <p className="text-xs font-medium text-warning">{t('preinstall.partial_failed', { count: failedIds.length })}</p>
                        <p className="text-xs leading-5 text-load-muted">{t('preinstall.partial_failed_hint')}</p>
                        <ul className="m-0 list-disc pl-5 text-xs leading-5 text-load-muted">
                          {failedPlugins.map(plugin => <li key={plugin.id}>{plugin.name}</li>)}
                        </ul>
                      </div>
                      <LogPanel logs={preinstall.logs} />
                      <div className="flex items-center justify-end gap-2">
                        <Button className="rounded-md" size="sm" variant="tertiary" onPress={handleSkip}>{t('preinstall.continue')}</Button>
                        <Button className="rounded-md" size="sm" variant="primary" onPress={handleRetryFailed}>{t('preinstall.retry_failed')}</Button>
                      </div>
                    </div>
                  )}
                  else={(
                    <>
                      <Card className="p-0 rounded-md">
                        <If cond={preinstall.plugins.length > 0} else={<p className="text-center text-xs text-load-muted">{t('preinstall.empty')}</p>}>
                          {preinstall.plugins.map(plugin => (
                            <PluginRow key={plugin.id} plugin={plugin} checked={effectiveSelected.has(plugin.id)} disabled={false} onToggle={toggle} onOpenRepo={openRepo} />
                          ))}
                        </If>
                      </Card>
                      <div className="flex items-center justify-end gap-2">
                        <Button className="rounded-md" size="sm" variant="tertiary" onPress={handleSkip}>{t('preinstall.skip')}</Button>
                        <Button className="rounded-md" size="sm" variant="primary" onPress={handleConfirm} isDisabled={selectedCount === 0 || selectableCount === 0}>{t('preinstall.confirm')}</Button>
                      </div>
                    </>
                  )}
                />
              )}
            />
          )}
        />
      </div>
    </div>
  )
}
