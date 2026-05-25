import { createBrowserRouter, Navigate } from "react-router-dom";

import { Shell } from "./layout/Shell";
import { Overview } from "./pages/Overview";
import { Hierarchy } from "./pages/Hierarchy";
import { SchemaList } from "./pages/SchemaList";
import { SchemaDetail } from "./pages/SchemaDetail";
import { SchemaEditor } from "./pages/SchemaEditor";
import { Records } from "./pages/Records";
import { RecordDetail } from "./pages/RecordDetail";
import { History } from "./pages/History";
import { Audit } from "./pages/Audit";
import { Health } from "./pages/Health";
import { Metrics } from "./pages/Metrics";
import { Logging } from "./pages/Logging";
import { AuthStrategyEditor } from "./pages/AuthStrategyEditor";
import { RoleBindingEditor } from "./pages/RoleBindingEditor";
import { ApiKeyEditor } from "./pages/ApiKeyEditor";
import { LogFilterEditor } from "./pages/LogFilterEditor";
import { LogRoutingEditor } from "./pages/LogRoutingEditor";
import { AdminTree } from "./pages/AdminTree";
import { NotFound } from "./pages/NotFound";

export const router = createBrowserRouter([
  {
    path: "/",
    element: <Shell />,
    children: [
      { index: true, element: <Navigate to="/overview" replace /> },
      { path: "overview", element: <Overview /> },
      { path: "hierarchy", element: <Hierarchy /> },
      { path: "schemas", element: <SchemaList /> },
      { path: "schemas/new", element: <SchemaEditor /> },
      // Schema/record paths use the five-segment Velocity addressing scheme.
      { path: "schemas/:org/:app/:domain/:object/:version", element: <SchemaDetail /> },
      { path: "schemas/:org/:app/:domain/:object/:version/edit", element: <SchemaEditor /> },
      { path: "records/:org/:app/:domain/:object/:version", element: <Records /> },
      { path: "records/:org/:app/:domain/:object/:version/:id", element: <RecordDetail /> },
      { path: "records/:org/:app/:domain/:object/:version/:id/history", element: <History /> },
      { path: "audit", element: <Audit /> },
      { path: "health", element: <Health /> },
      { path: "metrics", element: <Metrics /> },
      { path: "logging", element: <Logging /> },
      { path: "auth-strategies/new", element: <AuthStrategyEditor /> },
      { path: "role-bindings/new", element: <RoleBindingEditor /> },
      { path: "api-keys/new", element: <ApiKeyEditor /> },
      { path: "log-filters/new", element: <LogFilterEditor /> },
      { path: "log-routing/new", element: <LogRoutingEditor /> },
      { path: "admin", element: <AdminTree /> },
      { path: "admin/:kind/:namespace/:name", element: <AdminTree /> },
      { path: "*", element: <NotFound /> },
    ],
  },
]);
