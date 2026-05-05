import { useEffect, useState } from 'react';
import { Container } from './types';
import { ContainerCard } from './ContainerCard';
import './App.css';

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

  return (
    <div>
      <div className="header">
        <h1>
          Codery Deploy Console{' '}
          <span
            className={`conn-dot ${connected ? 'conn-live' : 'conn-dead'}`}
            title={connected ? 'Live' : 'Reconnecting…'}
          >●</span>
        </h1>
        <span className="header-sub">{import.meta.env.VITE_APP_VERSION ?? 'dev'}</span>
      </div>
      <div className="cards">
        {sorted.length === 0
          ? <p style={{ color: '#666' }}>Loading…</p>
          : sorted.map(c => <ContainerCard key={c.name} container={c} />)}
      </div>
      <div className="footer">
        Rollback: restarts stopped container · health-checks · flips Caddy routing
      </div>
    </div>
  );
}
