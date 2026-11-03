import { useState } from 'react'

/**
 * A destructive action that asks once, inline: the first press shows what
 * will happen and a confirm button; nothing is sent until that is pressed.
 */
export function ConfirmAction({
  label,
  confirmLabel,
  consequence,
  onConfirm,
  disabled,
  busy,
}: {
  label: string
  confirmLabel: string
  consequence: string
  onConfirm: () => void
  disabled?: boolean
  busy?: boolean
}) {
  const [asking, setAsking] = useState(false)
  if (!asking) {
    return (
      <button type="button" className="btn btn-danger" disabled={disabled || busy} onClick={() => setAsking(true)}>
        {label}
      </button>
    )
  }
  return (
    <span className="inline-flex flex-wrap items-center gap-2" role="group" aria-label={consequence}>
      <span className="text-xs text-bad">{consequence}</span>
      <button
        type="button"
        className="btn btn-danger"
        disabled={busy}
        onClick={() => {
          setAsking(false)
          onConfirm()
        }}
      >
        {confirmLabel}
      </button>
      <button type="button" className="btn btn-quiet" onClick={() => setAsking(false)}>
        Cancel
      </button>
    </span>
  )
}
