import { UI_BASE } from '@/config'

export const VIEWS = ['overview', 'credentials', 'identities', 'requests', 'plugin'] as const
export type View = (typeof VIEWS)[number]

export const VIEW_TITLES: Record<View, string> = {
  overview: 'Overview',
  credentials: 'Credentials',
  identities: 'Identities',
  requests: 'Requests',
  plugin: 'Plugin',
}

/** The view a location path names; anything unknown is the overview. */
export function viewFromPath(pathname: string, base: string = UI_BASE): View {
  const rest = pathname.startsWith(base) ? pathname.slice(base.length) : pathname.replace(/^\//, '')
  const first = rest.split('/')[0]
  return (VIEWS as readonly string[]).includes(first) ? (first as View) : 'overview'
}

export function pathForView(view: View, base: string = UI_BASE): string {
  return view === 'overview' ? base : `${base}${view}`
}
