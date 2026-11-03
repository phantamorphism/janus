/** The doorway Janus keeps: an arch split down the middle, one face each side. */
export function JanusMark({ className = '' }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className={className}
    >
      <path d="M5 21V10a7 7 0 0 1 14 0v11" />
      <path d="M12 3.2V21" />
      <path d="M3 21h18" />
    </svg>
  )
}
