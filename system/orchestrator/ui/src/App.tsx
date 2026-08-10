import { useEffect, useState } from 'react';
import { Container } from './types';
import { ContainerCard } from './ContainerCard';
import './App.css';

type SectionKey = 'sandbox' | 'apps' | '__other__';
const SECTION_ORDER: SectionKey[] = ['sandbox', 'apps', '__other__'];

const SECTION_LABEL: Record<SectionKey, string> = {
  sandbox: 'Sandbox',
  apps: 'Apps',
  __other__: 'Other',
};

function sectionFor(c: Container): SectionKey {
  const s = (c.service ?? '').toLowerCase();
  if (s === 'sandbox') return 'sandbox';
  if (s === 'apps') return 'apps';
  return '__other__';
}

export function App() {
  const [containers, setContainers] = useState<Container[]>([]);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    const es = new EventSource('/api/events');
    es.onmessage = (e) => {
      setConnected(true);
      try { setContainers(JSON.parse(e.data as string)); } catch { /* ignore malformed */ }
    };
    es.onerror = () => setConnected(false);
    return () => es.close();
  }, []);

  const alpha = (a: Container, b: Container) => a.name.localeCompare(b.name);
  const running  = containers.filter(c => c.state === 'running').sort(alpha);
  const inactive = containers.filter(c => c.state !== 'running').sort(alpha);
  const sorted   = [...running, ...inactive];

  const groups: Record<SectionKey, Container[]> = { sandbox: [], apps: [], __other__: [] };
  for (const c of sorted) groups[sectionFor(c)].push(c);
  const sections = SECTION_ORDER.filter(s => groups[s].length > 0);

  const runningCount = running.length;
  const exitedCount = inactive.length;

  return (
    <div className="page">
      <header className="header" data-od-id="dashboard-header">
        <div className="header-row">
          <div className="header-title">Codery Deploy Console</div>
          <div className="header-right">
            <span className="version">{import.meta.env.VITE_APP_VERSION ?? 'dev'}</span>
            <span
              className={`conn-pill ${connected ? 'live' : 'reconnecting'}`}
              data-od-id="connection-pill"
            >
              <span className="conn-dot" />
              {connected ? 'Live' : 'Reconnecting…'}
            </span>
          </div>
        </div>
        <div className="summary">
          <span className="summary-item">
            <span className="summary-dot" /> {runningCount} running
          </span>
          {exitedCount > 0 && (
            <span className="summary-item">
              <span className="summary-dot off" /> {exitedCount} exited
            </span>
          )}
        </div>
      </header>

      {sorted.length === 0
        ? <p className="loading">Loading…</p>
        : sections.map(s => (
            <section className="section" key={s} data-od-id={`section-${s}`}>
              <div className="section-label">{SECTION_LABEL[s]}</div>
              <div className="cards">
                {groups[s].map(c => <ContainerCard key={c.name} container={c} />)}
              </div>
            </section>
          ))}
    </div>
  );
}
