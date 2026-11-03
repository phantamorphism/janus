// The admin token lives in this tab's sessionStorage and nowhere else: not
// localStorage, not a cookie, not the URL (`docs/admin-surface#ui`). It is
// gone when the tab closes.

const KEY = 'janus.admin-token'

export function loadToken(): string | null {
  try {
    const t = window.sessionStorage.getItem(KEY)
    return t && t.trim() ? t : null
  } catch {
    return null
  }
}

export function saveToken(token: string): void {
  window.sessionStorage.setItem(KEY, token)
}

export function clearToken(): void {
  try {
    window.sessionStorage.removeItem(KEY)
  } catch {
    // Storage unavailable: nothing was kept.
  }
}
