// Where the UI is mounted and where the admin API lives.
//
// The daemon's UI listener serves the built UI at `/` and the admin API at
// `/v1` on the same origin. Vite's `base` (JANUS_UI_BASE at build time)
// moves the UI; the API follows it to `<base>v1` unless VITE_JANUS_API_BASE
// names a different same-origin prefix.

function trimTrailingSlash(s: string): string {
  return s.length > 1 && s.endsWith('/') ? s.slice(0, -1) : s
}

/** The UI's mount point, always ending in `/`. */
export const UI_BASE: string = import.meta.env.BASE_URL.endsWith('/')
  ? import.meta.env.BASE_URL
  : `${import.meta.env.BASE_URL}/`

/** The admin API prefix, without a trailing slash (default `/v1`). */
export const API_BASE: string = trimTrailingSlash(
  (import.meta.env.VITE_JANUS_API_BASE as string | undefined) || `${UI_BASE}v1`,
)
