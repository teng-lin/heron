/**
 * Top-level render error catcher.
 *
 * Without this, a thrown render in any descendant unmounts the whole
 * tree and leaves the user staring at the system body color — looks
 * exactly like an empty page. We surface the error message + stack so
 * "broken page" turns into "broken page with debuggable copy" without
 * having to open devtools.
 */

import { Component, type ErrorInfo, type ReactNode } from "react";

interface State {
  error: Error | null;
  info: ErrorInfo | null;
}

export class ErrorBoundary extends Component<{ children: ReactNode }, State> {
  state: State = { error: null, info: null };

  static getDerivedStateFromError(error: Error): State {
    return { error, info: null };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ error, info });
    // eslint-disable-next-line no-console
    console.error("[heron] render error:", error, info);
  }

  reset = () => this.setState({ error: null, info: null });

  render() {
    const { error, info } = this.state;
    if (error === null) return this.props.children;
    return (
      <div
        style={{
          padding: 24,
          fontFamily: "ui-monospace, SFMono-Regular, monospace",
          fontSize: 13,
          color: "var(--color-ink, #111)",
          background: "var(--color-paper, #fff)",
          height: "100vh",
          overflow: "auto",
        }}
      >
        <h1
          style={{
            fontSize: 18,
            fontWeight: 600,
            marginBottom: 12,
            color: "var(--color-rec, #c0392b)",
          }}
        >
          Something broke during render
        </h1>
        <p style={{ marginBottom: 8 }}>{String(error.message ?? error)}</p>
        <pre
          style={{
            whiteSpace: "pre-wrap",
            background: "var(--color-paper-2, #f5f5f5)",
            padding: 12,
            border: "1px solid var(--color-rule, #ddd)",
            borderRadius: 4,
            maxHeight: "60vh",
            overflow: "auto",
          }}
        >
          {error.stack ?? "(no stack)"}
          {info?.componentStack ?? ""}
        </pre>
        <div style={{ marginTop: 12, display: "flex", gap: 8 }}>
          <button
            type="button"
            onClick={this.reset}
            style={{
              padding: "6px 12px",
              border: "1px solid var(--color-rule, #ddd)",
              borderRadius: 4,
              background: "var(--color-paper, #fff)",
              cursor: "pointer",
            }}
          >
            Try again
          </button>
          <button
            type="button"
            onClick={() => window.location.reload()}
            style={{
              padding: "6px 12px",
              border: "1px solid var(--color-rule, #ddd)",
              borderRadius: 4,
              background: "var(--color-paper, #fff)",
              cursor: "pointer",
            }}
          >
            Reload
          </button>
        </div>
      </div>
    );
  }
}
