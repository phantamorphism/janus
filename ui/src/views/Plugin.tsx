import { useState } from 'react'
import { Check, RefreshCw, X } from 'lucide-react'
import { useApi } from '@/api/context'
import type { ChannelCheck, CheckStepKind } from '@/api/types'
import { PageHeader, Section } from '@/components/PageHeader'
import { ErrorNotice, Loading } from '@/components/States'
import { PluginFacts } from '@/components/PluginFacts'
import { octal, words } from '@/lib/format'
import { useResource } from '@/lib/useResource'

const stepLabel: Record<CheckStepKind, { title: string; expect: string }> = {
  path_permissions: {
    title: 'Socket path permissions',
    expect:
      'No directory on the socket path is writable by group or others, or owned by anyone but root, the daemon user, or the owner of the socket directory.',
  },
  connect: { title: 'Connection', expect: 'The plugin accepts a connection.' },
  handshake: {
    title: 'Handshake with the configured certificate',
    expect: 'The plugin accepts the daemon’s client certificate.',
  },
  throwaway_cert_rejected: {
    title: 'Throwaway certificate refused',
    expect: 'The plugin must reject a self-signed client certificate it has never seen.',
  },
  no_cert_rejected: {
    title: 'No certificate refused',
    expect: 'The plugin must reject a connection that presents no client certificate.',
  },
}

function CheckReport({ check }: { check: ChannelCheck }) {
  if (check.steps.length === 0) {
    return (
      <p className="text-sm text-muted" role="status">
        {check.mode === 'linked'
          ? 'A linked plugin runs inside the daemon, so there is no channel to check.'
          : 'The daemon ran no checks for this plugin mode.'}
      </p>
    )
  }
  return (
    <div>
      <p role="status" className={`mb-3 text-sm font-medium ${check.passed ? 'text-ok' : 'text-bad'}`}>
        {check.passed
          ? 'Every check passed: the channel is authenticated the way the config says.'
          : 'At least one check failed. Read the failing step before trusting this channel.'}
      </p>
      <ol className="divide-y divide-line rounded-md border border-line bg-surface" aria-label="Check steps">
        {check.steps.map((s, i) => {
          const label = stepLabel[s.step] ?? { title: words(s.step), expect: '' }
          return (
            <li key={`${s.step}-${i}`} className="flex gap-3 px-4 py-3" aria-label={label.title}>
              <span
                className={`mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full ${
                  s.passed ? 'bg-ok-soft text-ok' : 'bg-bad-soft text-bad'
                }`}
              >
                {s.passed ? <Check size={13} aria-hidden="true" /> : <X size={13} aria-hidden="true" />}
              </span>
              <div className="min-w-0 flex-1">
                <p className="text-sm font-medium">
                  {label.title}
                  <span className={`ml-2 text-xs font-normal ${s.passed ? 'text-ok' : 'text-bad'}`}>
                    {s.passed ? 'passed' : 'failed'}
                  </span>
                </p>
                {label.expect && <p className="text-xs text-muted">{label.expect}</p>}
                <p className="mono mt-1 break-words text-ink-soft">{s.detail}</p>
                {s.class && <p className="mt-1 text-xs text-bad">Failure class: {s.class}</p>}
                {s.walk && (
                  <table className="data-table mt-2">
                    <caption className="mb-1 text-left text-xs text-muted">
                      Walked from <span className="mono">{s.walk.socket}</span>
                    </caption>
                    <thead>
                      <tr>
                        <th scope="col">Directory</th>
                        <th scope="col">Owner uid</th>
                        <th scope="col">Mode</th>
                      </tr>
                    </thead>
                    <tbody>
                      {s.walk.dirs.map((d) => (
                        <tr key={d.path}>
                          <td className="mono">{d.path}</td>
                          <td className="mono">{d.uid}</td>
                          <td className="mono">{octal(d.mode)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                )}
              </div>
            </li>
          )
        })}
      </ol>
    </div>
  )
}

export function Plugin() {
  const api = useApi()
  const status = useResource((signal) => api.plugin(signal), 'plugin')
  const [check, setCheck] = useState<ChannelCheck | null>(null)
  const [checkError, setCheckError] = useState<unknown>(undefined)
  const [checking, setChecking] = useState(false)

  async function runCheck() {
    setChecking(true)
    setCheckError(undefined)
    setCheck(null)
    try {
      setCheck(await api.checkPlugin())
    } catch (e) {
      setCheckError(e)
    } finally {
      setChecking(false)
      status.reload()
    }
  }

  const plugin = status.data
  return (
    <>
      <PageHeader
        title="Plugin"
        lede="The policy plugin the daemon asks before each request, and whether the channel to it is really authenticated."
        actions={
          <button type="button" className="btn" onClick={status.reload} disabled={status.loading}>
            <RefreshCw size={14} aria-hidden="true" /> Reload
          </button>
        }
      />
      {status.error !== undefined && (
        <ErrorNotice error={status.error} what="plugin status" onRetry={status.reload} />
      )}
      {plugin === undefined ? (
        status.loading && <Loading what="plugin status" />
      ) : (
        <>
          <Section title="Channel">
            <PluginFacts plugin={plugin} />
          </Section>
          <Section title="Channel check">
            <p className="mb-3 max-w-prose text-sm text-muted">
              {plugin.mode === 'tls'
                ? 'Performs a full handshake with the configured certificate, then two handshakes the plugin must refuse: one with a throwaway certificate and one with none. This is the only way to confirm the plugin requires the daemon to authenticate.'
                : plugin.mode === 'unix'
                  ? 'Walks the permissions of every directory up to the socket, then connects.'
                  : plugin.mode === 'plaintext'
                    ? 'Attempts a connection. Plaintext has no authentication to verify.'
                    : plugin.mode === 'linked'
                      ? 'A linked plugin has no channel; the check reports that.'
                      : 'No plugin is configured, so there is no channel to check.'}
            </p>
            <button
              type="button"
              className="btn btn-primary"
              onClick={runCheck}
              disabled={checking || plugin.mode === 'none'}
            >
              {checking ? 'Checking…' : 'Run channel check'}
            </button>
            <div className="mt-4" aria-live="polite">
              {checkError !== undefined && <ErrorNotice error={checkError} what="the channel check" />}
              {check && <CheckReport check={check} />}
            </div>
          </Section>
        </>
      )}
    </>
  )
}
