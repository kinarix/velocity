import { NavLink } from "react-router-dom";

interface NavSection {
  title: string;
  items: { to: string; label: string }[];
}

const SECTIONS: NavSection[] = [
  {
    title: "Cluster",
    items: [
      { to: "/overview",  label: "Overview" },
      { to: "/hierarchy", label: "Hierarchy" },
      { to: "/health",    label: "Health" },
      { to: "/metrics",   label: "Metrics" },
    ],
  },
  {
    title: "Schemas",
    items: [
      { to: "/schemas",     label: "Schemas" },
      { to: "/schemas/new", label: "+ New schema" },
    ],
  },
  {
    title: "Access",
    items: [
      { to: "/auth-strategies/new", label: "+ AuthStrategy" },
      { to: "/role-bindings/new",   label: "+ RoleBinding" },
      { to: "/api-keys/new",        label: "+ ApiKey" },
    ],
  },
  {
    title: "Logging & Audit",
    items: [
      { to: "/logging",          label: "Central logging" },
      { to: "/log-filters/new",  label: "+ LogFilterPolicy" },
      { to: "/log-routing/new",  label: "+ LogRoutingPolicy" },
      { to: "/audit",            label: "Audit log" },
    ],
  },
];

export function Sidebar() {
  return (
    <nav className="w-56 shrink-0 border-r border-ink-700 bg-ink-900 overflow-y-auto py-2">
      {SECTIONS.map((section) => (
        <div key={section.title} className="mb-3">
          <div className="px-3 py-1 text-[10px] uppercase tracking-wider text-ink-400">
            {section.title}
          </div>
          {section.items.map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              className={({ isActive }) =>
                [
                  "block px-3 py-1.5 text-xs",
                  "hover:bg-ink-800",
                  isActive
                    ? "text-amber-400 border-l-2 border-amber-500 bg-ink-800"
                    : "text-ink-200 border-l-2 border-transparent",
                ].join(" ")
              }
            >
              {item.label}
            </NavLink>
          ))}
        </div>
      ))}
    </nav>
  );
}
