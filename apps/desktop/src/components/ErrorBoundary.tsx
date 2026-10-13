import { Component, type ErrorInfo, type ReactNode } from "react";

/**
 * Contains a crash to the panel it happened in. Without it, one throwing
 * component unmounts the whole React tree and the window turns black.
 * Shows what failed and a "Try again" that remounts the panel.
 */
export default class ErrorBoundary extends Component<{ children: ReactNode; label?: string; resetKey?: unknown }, { error: Error | null }> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(`[${this.props.label ?? "panel"}]`, error, info.componentStack);
  }

  componentDidUpdate(prev: { resetKey?: unknown }) {
    if (this.state.error && prev.resetKey !== this.props.resetKey) this.setState({ error: null });
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <div className="panel-crash" role="alert">
        <b>{this.props.label ?? "This panel"} stopped working.</b>
        <span className="mono">{String(this.state.error.message || this.state.error).slice(0, 300)}</span>
        <button type="button" className="btn small secondary" onClick={() => this.setState({ error: null })}>
          Try again
        </button>
      </div>
    );
  }
}
