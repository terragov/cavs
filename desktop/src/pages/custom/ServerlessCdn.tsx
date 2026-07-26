import { useState } from "react";
import { pickPath } from "../../api/client";
import { useI18n } from "../../i18n";
import { HelpPanel } from "../../components/HelpPanel";
import { CodeBlock } from "../../components/ui";
import { Icon } from "../../components/Icon";
import type { CustomPageProps } from "./types";

// A command-builder page for the v1.4.0 serverless / CDN flow: export a
// static tree from a store, then update clients from it with no cavs-server.
// Pure front end — it composes the CLI commands the user runs; it does not
// invoke a Tauri backend, so it works everywhere the app builds.
export function ServerlessCdn({ sectionId }: CustomPageProps) {
  const { section } = useI18n();
  const text = section(sectionId);

  const [store, setStore] = useState("./store");
  const [dist, setDist] = useState("./dist");
  const [base, setBase] = useState("https://cdn.example.com/dist");
  const [asset, setAsset] = useState("app");
  const [connections, setConnections] = useState(8);

  const q = (s: string) => (/\s/.test(s) ? `"${s}"` : s);

  const exportCmd = `cavs store ${q(store)} export --out ${q(dist)} --static-plans`;
  const uploadCmd = `# Upload the exported tree to any static host (Range-capable):\naws s3 sync ${q(dist)} s3://your-bucket/  # or: rclone copy, gh-pages, nginx docroot …`;
  const fetchCmd = `cavs-client fetch-static ${q(base)} ${asset} \\\n  -o ./install --cache ./cache --connections ${connections}`;

  const field = (
    label: string,
    value: string,
    set: (v: string) => void,
    browse?: "dir",
  ) => (
    <div className="field">
      <label>{label}</label>
      <div className="file-input">
        <input className="input mono" value={value} onChange={(e) => set(e.target.value)} />
        {browse && (
          <button
            className="btn"
            onClick={async () => {
              const p = await pickPath({ directory: true });
              if (p) set(p);
            }}
          >
            <Icon name="folder-open" size={15} /> Browse
          </button>
        )}
      </div>
    </div>
  );

  return (
    <div className="content-inner">
      <div className="page-head">
        <div>
          <h1 className="page-title">{text.label}</h1>
          <p className="page-tagline">{text.tagline}</p>
        </div>
      </div>
      <HelpPanel sectionId={sectionId} />

      <div className="card" style={{ marginBottom: 16 }}>
        <div className="subhead">1 · Export a static tree</div>
        {field("Store folder", store, setStore, "dir")}
        {field("Output (dist) folder", dist, setDist, "dir")}
        <CodeBlock lang="bash" code={exportCmd} />
        <p className="text-dim" style={{ fontSize: 13, marginTop: 8 }}>
          Writes immutable <code>.cavspack</code> files plus per-asset{" "}
          <code>manifest.json</code> and <code>chunk-map.json</code>. Requires a
          packfile-layout store (<code>store add … --storage packfiles</code>).
        </p>
      </div>

      <div className="card" style={{ marginBottom: 16 }}>
        <div className="subhead">2 · Upload</div>
        <CodeBlock lang="bash" code={uploadCmd} />
      </div>

      <div className="card" style={{ marginBottom: 16 }}>
        <div className="subhead">3 · Update a client (no server)</div>
        {field("Base URL or folder", base, setBase)}
        <div className="row" style={{ gap: 14, alignItems: "flex-end" }}>
          <div className="field" style={{ flex: 1, marginBottom: 0 }}>
            <label>Asset</label>
            <input className="input mono" value={asset} onChange={(e) => setAsset(e.target.value)} />
          </div>
          <div className="field" style={{ width: 160, marginBottom: 0 }}>
            <label>Parallel connections</label>
            <input
              className="input"
              type="number"
              min={1}
              max={32}
              value={connections}
              onChange={(e) => setConnections(Number(e.target.value))}
            />
          </div>
        </div>
        <CodeBlock lang="bash" code={fetchCmd} />
        <p className="text-dim" style={{ fontSize: 13, marginTop: 8 }}>
          The client plans the missing set locally and downloads only changed
          chunks over {connections} concurrent HTTP Range requests, verified end
          to end. The same engine is available in the SDKs (<code>fetchStatic</code>)
          and the Unity/Unreal plugins for in-game self-update.
        </p>
      </div>
    </div>
  );
}
