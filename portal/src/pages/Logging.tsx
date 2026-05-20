import { Link } from "react-router-dom";

import { PageTitle } from "../components/PageTitle";

export function Logging() {
  return (
    <>
      <PageTitle
        title="Central logging"
        subtitle="velocity-log-collector → log-processor → sinks (Kafka / S3 / Loki / Elastic)."
      />

      <div className="grid grid-cols-2 gap-3">
        <div className="card p-4">
          <div className="panel-title -m-4 mb-3">Pipeline</div>
          <ol className="text-xs space-y-2 list-decimal list-inside text-ink-200">
            <li>
              <code className="text-amber-400">velocity-log-collector</code> DaemonSet tails container
              stdout on every node.
            </li>
            <li>
              <code className="text-amber-400">velocity-log-processor</code> enriches with cluster
              metadata, applies <code>LogFilterPolicy</code> rules, then fan-outs per
              <code> LogRoutingPolicy</code>.
            </li>
            <li>Each route lands in its sink (Kafka, S3, Loki, Elastic, HTTP).</li>
          </ol>
        </div>

        <div className="card p-4">
          <div className="panel-title -m-4 mb-3">Manage policies</div>
          <ul className="text-xs space-y-2">
            <li>
              <Link className="text-amber-400 hover:underline" to="/log-filters/new">
                + New LogFilterPolicy
              </Link>
              <span className="text-ink-400"> — drop / redact / sample rules</span>
            </li>
            <li>
              <Link className="text-amber-400 hover:underline" to="/log-routing/new">
                + New LogRoutingPolicy
              </Link>
              <span className="text-ink-400"> — sink fan-out per tenant</span>
            </li>
          </ul>
        </div>
      </div>

      <div className="card mt-3 p-4 text-xs text-ink-300">
        <p>
          A live log stream view requires a websocket endpoint that doesn't exist yet on{" "}
          <code>velocity-api</code>. For now, stream logs through your central sink (Loki/Elastic) or
          via <code>velocity logs &lt;pod&gt;</code> in the CLI.
        </p>
      </div>
    </>
  );
}
