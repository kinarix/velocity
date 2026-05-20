import { Outlet } from "react-router-dom";

import { Sidebar } from "./Sidebar";
import { Header } from "./Header";

/**
 * Three-panel shell:
 *   [ Sidebar | Main content | (page-specific right panel optional) ]
 * Sidebar is fixed-width. Main content scrolls.
 */
export function Shell() {
  return (
    <div className="h-full flex flex-col">
      <Header />
      <div className="flex-1 flex min-h-0">
        <Sidebar />
        <main className="flex-1 overflow-auto min-w-0">
          <div className="p-4 max-w-screen-2xl mx-auto">
            <Outlet />
          </div>
        </main>
      </div>
    </div>
  );
}
