import { Link } from "react-router-dom";

export function NotFound() {
  return (
    <div className="card p-6 text-center">
      <div className="text-amber-400 text-3xl font-bold mb-2">404</div>
      <div className="text-ink-300 text-xs mb-4">No such route.</div>
      <Link to="/overview" className="btn btn-primary">
        Back to overview
      </Link>
    </div>
  );
}
