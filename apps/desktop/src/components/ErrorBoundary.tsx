import { Component, type ErrorInfo, type ReactNode, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { copyText } from "../lib/clipboard";

interface Props {
  children: ReactNode;
  /** "app" = full-window fallback (top level); "view" = inline fallback for one tab. */
  scope?: "app" | "view";
}

interface State {
  error: Error | null;
  componentStack: string | null;
}

/**
 * Catches render errors so a bad payload or a webview incompatibility shows a
 * recoverable screen instead of unmounting the whole tree (a blank window, or at launch
 * a splash that never goes away). Wrap the app once at the top (`scope="app"`) and each
 * view (`scope="view"`, keyed by view) so one broken tab doesn't take down the rest.
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, componentStack: null };

  static getDerivedStateFromError(error: unknown): Partial<State> {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error("[ui] render error:", error, info.componentStack);
    this.setState({ componentStack: info.componentStack ?? null });
  }

  private reset = () => this.setState({ error: null, componentStack: null });

  render() {
    const { error, componentStack } = this.state;
    if (!error) return this.props.children;
    return (
      <CrashScreen
        error={error}
        componentStack={componentStack}
        scope={this.props.scope ?? "app"}
        onRetry={this.reset}
      />
    );
  }
}

function diagnostics(error: Error, componentStack: string | null): string {
  return [
    `Callimachus UI error: ${error.name}: ${error.message}`,
    `User agent: ${navigator.userAgent}`,
    "",
    "Stack:",
    error.stack ?? "(none)",
    "",
    "Component stack:",
    componentStack?.trim() ?? "(none)",
  ].join("\n");
}

function CrashScreen({
  error,
  componentStack,
  scope,
  onRetry,
}: {
  error: Error;
  componentStack: string | null;
  scope: "app" | "view";
  onRetry: () => void;
}) {
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");

  // The main window starts hidden behind the splash and is revealed only once the
  // frontend reports ready (main.tsx). A crash before that would otherwise leave the
  // user stuck on the splash, so report ready from here too (idempotent backend-side).
  useEffect(() => {
    void invoke("set_complete", { task: "frontend" }).catch(() => {});
  }, []);

  const copy = () => {
    copyText(diagnostics(error, componentStack)).then(
      () => setCopied("copied"),
      () => setCopied("failed"),
    );
  };

  const button =
    "inline-flex h-8 cursor-pointer items-center rounded-lg border px-3 text-sm font-medium transition-colors hover:bg-muted";

  return (
    <div
      role="alert"
      className={
        scope === "app"
          ? "flex h-screen flex-col items-center justify-center gap-4 bg-background p-8 text-foreground"
          : "flex min-h-0 flex-1 flex-col items-center justify-center gap-4 p-8"
      }
    >
      <div className="max-w-lg space-y-1.5 text-center">
        <h1 className="text-lg font-semibold">Something went wrong</h1>
        <p className="text-sm text-muted-foreground">
          {scope === "app"
            ? "Callimachus hit an unexpected error while drawing this window. Reloading usually fixes it; your index is not affected."
            : "This view hit an unexpected error. Try again, switch to another tab, or reload the window."}
        </p>
        <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-md bg-muted p-3 text-left text-xs">
          {error.message || error.name}
        </pre>
      </div>
      <div className="flex flex-wrap items-center justify-center gap-2">
        {scope === "view" && (
          <button type="button" className={button} onClick={onRetry}>
            Try again
          </button>
        )}
        <button
          type="button"
          className={`${button} bg-primary text-primary-foreground hover:bg-primary/90`}
          onClick={() => window.location.reload()}
        >
          Reload
        </button>
        <button type="button" className={button} onClick={copy}>
          {copied === "copied"
            ? "Copied"
            : copied === "failed"
              ? "Copy failed"
              : "Copy diagnostics"}
        </button>
      </div>
    </div>
  );
}
