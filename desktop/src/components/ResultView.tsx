import { api, errMessage } from "../api/client";
import type { OperationRecord } from "../api/types";
import { useI18n } from "../i18n";
import { useStore } from "../app/store";
import { basename, formatBytes, formatPercent } from "../lib/format";
import { cliEquivalent } from "../lib/cli";
import { BarChart, Donut, ReuseHeatmap, type BarItem } from "./charts";
import { CodeBlock } from "./ui";
import { Icon } from "./Icon";

const BYTE_HINT = /bytes|size|download|read|written|temp|ram/i;

export function ResultView({ record }: { record: OperationRecord }) {
  const { t } = useI18n();
  const { settings, notify } = useStore();

  const openFolder = async () => {
    try {
      await api.openPath(record.artifactDir);
    } catch (e) {
      notify("error", errMessage(e));
    }
  };

  const cli = cliEquivalent(record.kind, record.params ?? {});

  return (
    <div>
      {record.status === "failed" && record.error && (
        <div className="rec critical" style={{ marginBottom: 16 }}>
          <h4>
            {record.error.code}
          </h4>
          <p>{record.error.message}</p>
        </div>
      )}

      {/* Generated files */}
      <div className="subhead">{t("result.outputs")}</div>
      {record.files.length > 0 ? (
        <div className="card" style={{ marginBottom: 8 }}>
          <div className="row spread">
            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              {record.files.map((f) => (
                <span key={f} className="mono">{f}</span>
              ))}
            </div>
            <button className="btn" onClick={openFolder}>
              <Icon name="folder-open" size={15} />
              {t("common.openFolder")}
            </button>
          </div>
        </div>
      ) : (
        <div className="row spread card" style={{ marginBottom: 8 }}>
          <span className="text-dim">{t("result.noFiles")}</span>
          <button className="btn btn-ghost" onClick={openFolder}>
            <Icon name="folder-open" size={15} />
            {t("common.openFolder")}
          </button>
        </div>
      )}

      {record.status === "completed" && (
        <OperationBody kind={record.kind} result={record.result} />
      )}

      {record.status === "completed" && record.section === "godot-runtime" && (
        <GodotSnippet
          asset={record.params?.assetName ?? "game_content"}
          version={record.params?.newVersion ?? "1.0.1"}
        />
      )}

      {settings.showCliPreview && cli && (
        <>
          <div className="subhead">{t("result.cli")}</div>
          <CodeBlock code={cli} lang="bash" />
        </>
      )}

      <details style={{ marginTop: 16 }}>
        <summary style={{ cursor: "pointer", color: "var(--text-dim)", fontSize: 12.5 }}>
          {t("result.raw")}
        </summary>
        <pre className="code" style={{ marginTop: 8 }}>
          {JSON.stringify({ params: record.params, result: record.result }, null, 2)}
        </pre>
      </details>
    </div>
  );
}

function OperationBody({ kind, result }: { kind: string; result: any }) {
  if (!result || typeof result !== "object") return null;
  if (kind === "analyze") return <AnalyzeBody r={result} />;
  if (kind === "previewUpdate" || kind === "compareRoutes") return <RoutesBody r={result} />;
  return <GenericBody r={result} />;
}

function AnalyzeBody({ r }: { r: any }) {
  const { t } = useI18n();
  const s = r.summary ?? {};
  const savings =
    s.newSizeBytes && s.estimatedUpdateBytes != null
      ? (1 - s.estimatedUpdateBytes / Math.max(1, s.newSizeBytes)) * 100
      : null;

  const sizeBars: BarItem[] = [
    { label: "Full download", value: s.newSizeBytes ?? 0, color: "gray" },
    { label: "SteamPipe est.", value: s.estimatedSteamPipeBytes ?? 0, color: "yellow" },
    { label: "CAVS update", value: s.estimatedUpdateBytes ?? 0, color: "green" },
  ];

  const worst: any[] = Array.isArray(s.worstFiles) ? s.worstFiles : [];
  const heat = worst.slice(0, 24).map((f) => ({
    ratio: f.reuseRatio ?? 0,
    width: Math.max(1, f.newSizeBytes ?? f.oldSizeBytes ?? 1),
    label: basename(f.path ?? ""),
  }));

  return (
    <>
      <div className="subhead">{t("result.summary")}</div>
      <div className="card-grid grid-3" style={{ marginBottom: 8 }}>
        <Stat label="Full download" value={formatBytes(s.newSizeBytes)} />
        <Stat label="CAVS update" value={formatBytes(s.estimatedUpdateBytes)} />
        <Stat
          label="Files changed"
          value={String((s.filesModified ?? 0) + (s.filesAdded ?? 0) + (s.filesDeleted ?? 0))}
          sub={`${s.filesModified ?? 0} mod · ${s.filesAdded ?? 0} add · ${s.filesDeleted ?? 0} del`}
        />
      </div>

      <div className="card" style={{ marginBottom: 8 }}>
        <div className="row spread wrap" style={{ gap: 20 }}>
          {savings != null && <Donut percent={savings} label="Estimated savings" />}
          <div style={{ flex: 1, minWidth: 260 }}>
            <BarChart items={sizeBars} />
          </div>
        </div>
      </div>

      {heat.length > 0 && (
        <div className="card" style={{ marginBottom: 8 }}>
          <div className="subhead" style={{ marginTop: 0 }}>Changed-region map</div>
          <ReuseHeatmap segments={heat} />
        </div>
      )}

      {Array.isArray(r.recommendations) && r.recommendations.length > 0 && (
        <>
          <div className="subhead">{t("result.recommendations")}</div>
          {r.recommendations.map((rec: any, i: number) => (
            <div key={i} className={"rec " + severityClass(rec.severity)}>
              <h4>{rec.title}</h4>
              {rec.why && <p>{rec.why}</p>}
              {rec.file && <p className="mono">{rec.file}</p>}
              {rec.estimatedWastedBytes > 0 && (
                <p>Wasted ≈ {formatBytes(rec.estimatedWastedBytes)}</p>
              )}
              {rec.fix && <p className="rec-fix">→ {rec.fix}</p>}
            </div>
          ))}
        </>
      )}

      {worst.length > 0 && (
        <>
          <div className="subhead">Per-file cost</div>
          <div className="table-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>File</th>
                  <th>Status</th>
                  <th>New size</th>
                  <th>CAVS</th>
                  <th>Reuse</th>
                </tr>
              </thead>
              <tbody>
                {worst.map((f: any, i: number) => (
                  <tr key={i}>
                    <td className="mono" title={f.path}>{basename(f.path ?? "")}</td>
                    <td>{f.status}</td>
                    <td>{formatBytes(f.newSizeBytes)}</td>
                    <td>{formatBytes(f.estimatedDownloadBytes)}</td>
                    <td>{formatPercent(f.reuseRatio)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </>
  );
}

function RoutesBody({ r }: { r: any }) {
  const { t } = useI18n();
  const routes: any[] = Array.isArray(r.routes) ? r.routes : [];
  if (routes.length === 0) return <GenericBody r={r} />;
  const bars: BarItem[] = routes.map((rt) => ({
    label: rt.name ?? rt.route ?? "route",
    value: rt.networkBytes ?? rt.downloadBytes ?? 0,
    color: (rt.recommended ? "green" : "accent") as BarItem["color"],
  }));
  return (
    <>
      <div className="subhead">{t("result.routes")}</div>
      <div className="card" style={{ marginBottom: 8 }}>
        <BarChart items={bars} />
      </div>
      <div className="table-wrap">
        <table className="tbl">
          <thead>
            <tr>
              <th>Route</th>
              <th>Download</th>
              <th>Notes</th>
            </tr>
          </thead>
          <tbody>
            {routes.map((rt, i) => (
              <tr key={i}>
                <td>{rt.name ?? rt.route}</td>
                <td>{formatBytes(rt.networkBytes ?? rt.downloadBytes)}</td>
                <td className="text-dim">
                  {Array.isArray(rt.notes) ? rt.notes.join("; ") : rt.notes ?? ""}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

function GenericBody({ r }: { r: any }) {
  const { t } = useI18n();
  const entries = flattenPrimitives(r);
  if (entries.length === 0) return null;
  return (
    <>
      <div className="subhead">{t("result.summary")}</div>
      <div className="card">
        <dl className="kv">
          {entries.map(([k, v]) => (
            <div key={k} style={{ display: "contents" }}>
              <dt>{humanize(k)}</dt>
              <dd>{BYTE_HINT.test(k) && typeof v === "number" ? formatBytes(v) : String(v)}</dd>
            </div>
          ))}
        </dl>
      </div>
    </>
  );
}

function GodotSnippet({ asset, version }: { asset: string; version: string }) {
  const snippet = `Cavs.configure({
    "server_url": "http://localhost:8990",
    "cache_dir": "user://cavs_cache",
    "packs_dir": "user://packs"
})

var result = await Cavs.update_and_mount("${asset}", "${version}")
if result.ok:
    print("Updated and mounted")
else:
    push_error(result.error)`;
  return (
    <>
      <div className="subhead">Godot snippet</div>
      <CodeBlock code={snippet} lang="gdscript" />
      <p className="text-dim" style={{ fontSize: 12, marginTop: 8 }}>
        Start the local server (Local Server section) pointed at this operation's folder, then paste
        this snippet into your application.
      </p>
    </>
  );
}

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="stat">
      <div className="stat-label">{label}</div>
      <div className="stat-value">{value}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </div>
  );
}

function severityClass(sev: string): string {
  if (sev === "critical" || sev === "error") return "critical";
  if (sev === "warning") return "warning";
  return "info";
}

function flattenPrimitives(obj: any): [string, any][] {
  const out: [string, any][] = [];
  for (const [k, v] of Object.entries(obj ?? {})) {
    if (v == null) continue;
    if (typeof v === "object") continue;
    out.push([k, v]);
  }
  return out;
}

function humanize(k: string): string {
  return k
    .replace(/([a-z])([A-Z])/g, "$1 $2")
    .replace(/^./, (c) => c.toUpperCase());
}
