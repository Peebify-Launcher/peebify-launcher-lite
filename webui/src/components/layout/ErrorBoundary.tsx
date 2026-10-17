// ------------ Error Boundary ------------
// Catches a crash while drawing the interface, writes it to the log and shows the caller's fallback screen
// instead of a blank window.
import { Component, type ErrorInfo, type ReactNode } from "react";
import { log } from "../../lib/log";

interface Props {
  children: ReactNode;
  fallback: (reset: () => void) => ReactNode;
  scope?: string;
  context?: () => Record<string, unknown>;
}

interface State {
  error: Error | null;
}

function readContext(context: Props["context"]): Record<string, unknown> | string {
  if (!context) return "";
  try {
    return context();
  } catch (e) {
    return `(context unavailable: ${String(e)})`;
  }
}

const RETRY_WINDOW_MS = 10_000;
let lastRetryAt = 0;

export function retryOrReload(reset: () => void): void {
  if (Date.now() - lastRetryAt < RETRY_WINDOW_MS) {
    window.location.reload();
    return;
  }
  lastRetryAt = Date.now();
  reset();
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    const scope = this.props.scope ? ` (${this.props.scope})` : "";
    log.error(
      `React render crash${scope}:`,
      error,
      readContext(this.props.context),
      info.componentStack || "",
    );
    void log.flush();
  }

  reset = (): void => this.setState({ error: null });

  render(): ReactNode {
    if (!this.state.error) return this.props.children;
    return this.props.fallback(this.reset);
  }
}
