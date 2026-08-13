import { Component, type ReactNode } from "react"

/// Renders children; a render error swaps in a visible error card instead
/// of unmounting the whole tree (React's default → blank/dark window).
/// `key` on the boundary resets it after a recovery (route change, save).
export class ErrorBoundary extends Component<
  { children: ReactNode; onReset?: () => void },
  { error: Error | null }
> {
  state = { error: null as Error | null }

  static getDerivedStateFromError(error: Error) {
    return { error }
  }

  private reset = () => {
    this.setState({ error: null })
    this.props.onReset?.()
  }

  render() {
    if (this.state.error === null) return this.props.children
    const err = this.state.error
    return (
      <div className="mx-auto my-10 max-w-xl border border-danger/40 bg-danger/5 p-4">
        <div className="text-sm font-semibold text-danger">render error</div>
        <pre className="mt-2 overflow-auto whitespace-pre-wrap font-mono text-xs text-muted">
          {err.message || String(err)}
        </pre>
        <button
          type="button"
          className="mt-3 text-xs text-accent underline"
          onClick={this.reset}
        >
          retry
        </button>
      </div>
    )
  }
}
