import { useEffect, useRef, useState } from 'react';
import { HostMetrics, TopProcess } from './types';

const HEALTH_LABEL = { green: 'Healthy', yellow: 'Strained', red: 'Critical' } as const;
const fmt = (n: number) => n.toLocaleString('en-US');

const memTone = (availPct: number) =>
  availPct < 10 ? 'membar--danger' : availPct < 20 ? 'membar--warn' : '';
const psiTone = (v: number) => (v >= 10 ? 'psi--danger' : v >= 2 ? 'psi--warn' : 'psi--calm');
const swapTone = (pct: number) =>
  pct > 60 ? 'swapbar--danger' : pct > 25 ? 'swapbar--warn' : 'swapbar--ok';

function Pill({ m }: { m: HostMetrics }) {
  const h = m.health;
  return (
    <span className={`pill pill--${h.status}`} data-od-id="host-health-pill"
      title={h.reasons.join(' · ') || 'No active triggers'}>
      <i className="pill-dot" />{HEALTH_LABEL[h.status]}
    </span>
  );
}

function Reasons({ m }: { m: HostMetrics }) {
  if (m.health.reasons.length === 0) return null;
  return (
    <ul className="pill-reasons" data-od-id="host-health-reasons">
      {m.health.reasons.map((r) => <li key={r}>{r}</li>)}
    </ul>
  );
}

function Oom({ m, bump }: { m: HostMetrics; bump: boolean }) {
  return (
    <div className={`oom ${m.oom_kills > 0 ? 'oom--hot' : ''}`} data-od-id="host-oom-badge">
      <i className="oom-dot" />
      <span className="mono oom-count">{m.oom_kills}</span>
      <span>OOM kills since boot</span>
      {bump && <span className="oom-bump" data-od-id="host-oom-bump">+1</span>}
    </div>
  );
}

function MemorySection({ m }: { m: HostMetrics }) {
  const { memory: mem } = m;
  const used = Math.max(mem.total_mb - mem.available_mb, 0);
  const usedPct = mem.total_mb > 0 ? (used / mem.total_mb) * 100 : 0;
  const cachePct = mem.total_mb > 0 ? ((mem.cached_mb + mem.buffers_mb) / mem.total_mb) * 100 : 0;
  const cacheInUsed = usedPct > 0 ? (cachePct / usedPct) * 100 : 0;
  const swapUsed = Math.max(mem.swap_total_mb - mem.swap_free_mb, 0);
  const swapPct = mem.swap_total_mb > 0 ? (swapUsed / mem.swap_total_mb) * 100 : 0;
  return (
    <>
      <div className={`membar ${memTone((mem.available_mb / mem.total_mb) * 100)}`}
        data-od-id="host-memory-bar"
        role="img" aria-label={`${fmt(used)} MB used of ${fmt(mem.total_mb)} MB`}>
        <div className="membar-used" style={{ width: `${usedPct.toFixed(1)}%` }}>
          <div className="membar-cached" title="cached + buffers"
            style={{ width: `${Math.min(cacheInUsed, 100).toFixed(1)}%` }} />
        </div>
      </div>
      <div className="mem-nums" data-od-id="host-memory-nums">
        <span className="mono big">{fmt(mem.available_mb)} MB</span> <span className="mut">free</span>
        <span className="mut"> · {fmt(mem.total_mb)} MB total</span>
      </div>
      <div className="mem-note mut">
        cache + buffers {fmt(mem.cached_mb + mem.buffers_mb)} MB reclaimable (hatched)
      </div>
      {mem.swap_total_mb > 0 ? (
        <div className="swap" data-od-id="host-swap">
          <div className={`swapbar ${swapTone(swapPct)}`}><i style={{ width: `${swapPct.toFixed(1)}%` }} /></div>
          <span className="mono">{fmt(swapUsed)}</span>
          <span className="mut">/ {fmt(mem.swap_total_mb)} MB swap</span>
        </div>
      ) : (
        <div className="swap" data-od-id="host-swap">
          <span className="tag tag--warn">NO SWAP</span>
          <span className="mut">swap disabled on this host</span>
        </div>
      )}
    </>
  );
}

function PsiRow({ name, r, id }: { name: string; r: { some: { avg10: number; avg60: number; avg300: number } }; id: string }) {
  const a = r.some;
  return (
    <div className={`psi-row ${psiTone(a.avg10)}`} data-od-id={`host-psi-${id}`}
      title={`avg60 ${a.avg60}% · avg300 ${a.avg300}% (5-min)`}>
      <span className="psi-name">{name}</span>
      <span className="psi-bar">
        <i className="psi-fill" style={{ width: `${Math.min(100, a.avg10).toFixed(1)}%` }} />
        <i className="psi-tick" style={{ left: `${Math.min(100, a.avg60).toFixed(1)}%` }} />
      </span>
      <span className="psi-val mono">{a.avg10.toFixed(1)}</span>
    </div>
  );
}

function PsiSection({ m }: { m: HostMetrics }) {
  if (!m.psi) {
    return (
      <div className="hh-empty" data-od-id="host-psi-placeholder">
        <b>PSI unavailable</b>
        <span>kernel doesn't expose pressure stall reports — CPU / memory / IO stall can't be shown</span>
      </div>
    );
  }
  return (
    <>
      <PsiRow name="CPU" r={m.psi.cpu} id="cpu" />
      <PsiRow name="Memory" r={m.psi.memory} id="memory" />
      <PsiRow name="IO" r={m.psi.io} id="io" />
    </>
  );
}

function OffendersSection({ m }: { m: HostMetrics }) {
  const procs = m.top_processes;
  const max = Math.max(...procs.map((p) => p.rss_mb), 1);
  const [open, setOpen] = useState(false);
  return (
    <>
      <div className="thead"><span>process</span><span></span><span>rss</span><span>cpu%</span></div>
      <div className={`twrap${open ? ' open' : ''}`} data-tablewrap>
        {procs.map((p: TopProcess, i) => {
          const tiny = p.rss_mb < max * 0.05;
          const hide = i >= 4 ? ' trow--hide' : '';
          const badge = p.container.startsWith('codery-')
            ? p.container.replace(/^codery-/, '')
            : p.container;
          return (
            <div key={`${p.pid}-${p.comm}`} className={`trow${tiny ? ' trow--tiny' : ''}${hide}`}>
              <span className="tname">
                <span className="pname">{p.comm}</span>
                <span className={`pbadge pbadge--${p.container === 'host' ? 'host' : 'ctr'}`}>{badge}</span>
              </span>
              <span className="tbar"><i style={{ width: `${((p.rss_mb / max) * 100).toFixed(1)}%` }} /></span>
              <span className="trss mono">{p.rss_mb.toFixed(0)}</span>
              <span className="tcpu mono">{p.cpu_pct.toFixed(1)}</span>
            </div>
          );
        })}
      </div>
      {procs.length > 4 && (
        <button className="tmore" data-toggle-rows onClick={() => setOpen(!open)}>
          {open ? 'Show top 4' : `Show all ${procs.length}`}
        </button>
      )}
    </>
  );
}

export function HostRail({ metrics }: { metrics: HostMetrics | null }) {
  const [bump, setBump] = useState(false);
  const prevOom = useRef<number | null>(null);
  useEffect(() => {
    if (!metrics) return;
    const prev = prevOom.current;
    prevOom.current = metrics.oom_kills;
    if (prev !== null && metrics.oom_kills > prev) {
      setBump(true);
      const t = setTimeout(() => setBump(false), 5000);
      return () => clearTimeout(t);
    }
  }, [metrics]);

  if (!metrics) {
    return (
      <aside className="rail rail--a" data-od-id="host-health-rail">
        <section className="hh-section"><p className="hh-empty"><b>Loading metrics…</b></p></section>
      </aside>
    );
  }
  return (
    <aside className="rail rail--a" data-od-id="host-health-rail">
      <section className="hh-section">
        <div className="hh-label">Status</div>
        <Pill m={metrics} />
        <Reasons m={metrics} />
        <Oom m={metrics} bump={bump} />
      </section>
      <section className="hh-section">
        <div className="hh-label">Memory</div>
        <MemorySection m={metrics} />
      </section>
      <section className="hh-section">
        <div className="hh-label">Pressure · PSI<span className="hh-legend">fill avg10 · tick avg60 · hover 5-min</span></div>
        <PsiSection m={metrics} />
      </section>
      <section className="hh-section">
        <div className="hh-label">Offenders<span className="hh-legend">top {metrics.top_processes.length} by RSS</span></div>
        <OffendersSection m={metrics} />
      </section>
    </aside>
  );
}
