import { Outlet } from "react-router-dom";

import { Sidebar } from "./sidebar";
import { TitleBar } from "./title-bar";

/**
 * Wraps the authenticated route tree with the new chrome (TitleBar +
 * Sidebar). `/onboarding` does NOT mount inside this layout — the
 * wizard owns the full window until the user finishes setup.
 */
export function RootLayout() {
  return (
    <div className="flex h-screen flex-col overflow-hidden">
      <TitleBar />
      <div className="flex flex-1 min-h-0">
        <Sidebar />
        <main
          className="flex-1 overflow-auto"
          style={{ background: "var(--color-paper)" }}
        >
          <Outlet />
        </main>
      </div>
    </div>
  );
}
