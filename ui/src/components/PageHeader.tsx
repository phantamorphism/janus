import type { ReactNode } from 'react'

export function PageHeader({
  title,
  lede,
  actions,
}: {
  title: string
  lede: ReactNode
  actions?: ReactNode
}) {
  return (
    <header className="mb-6 flex flex-wrap items-end justify-between gap-4 border-b border-line pb-4">
      <div className="max-w-prose">
        <h1 className="text-3xl leading-tight">{title}</h1>
        <p className="mt-1 text-sm text-muted">{lede}</p>
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </header>
  )
}

export function Section({ title, children, aside }: { title: string; children: ReactNode; aside?: ReactNode }) {
  return (
    <section className="mb-10" aria-label={title}>
      <div className="mb-3 flex items-baseline justify-between gap-4">
        <h2 className="text-xl">{title}</h2>
        {aside}
      </div>
      {children}
    </section>
  )
}
