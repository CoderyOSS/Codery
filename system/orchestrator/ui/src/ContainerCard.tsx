import { useState } from 'react';
import { Container } from './types';

interface Props {
  container: Container;
}

export function ContainerCard({ container: c }: Props) {
  const [localOp,  setLocalOp]  = useState<string | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  // Server operation takes precedence; localOp bridges the gap before SSE responds.
  const operation = c.operation ?? localOp;
  const inOp      = Boolean(operation);

  const cardClasses = [
    'card',
    c.state !== 'running' ? 'inactive'    : '',
    inOp                  ? 'in-progress' : '',
    errorMsg              ? 'error'       : '',
  ].filter(Boolean).join(' ');

  const opLabel = operation === 'rolling_back' ? '↩ Rolling back…'
                : operation === 'restarting'   ? '↺ Restarting…'
                : null;

  async function doFetch(url: string): Promise<{ ok: boolean; text: string }> {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 10_000);
    try {
      const resp = await fetch(url, { method: 'POST', signal: ctrl.signal });
      const text = resp.ok ? '' : ((await resp.text()) || `HTTP ${resp.status}`);
      return { ok: resp.ok, text };
    } catch (err) {
      const msg = err instanceof Error && err.name === 'AbortError'
        ? 'timed out (no response after 10s)'
        : 'network error';
      return { ok: false, text: msg };
    } finally {
      clearTimeout(timer);
    }
  }

  async function handleRestart() {
    console.log('[ui] restart clicked, container=', c.name);
    setErrorMsg(null);
    setLocalOp('restarting');
    const { ok, text } = await doFetch(`/api/restart/${encodeURIComponent(c.name)}`);
    console.log('[ui] restart response ok=', ok, 'text=', text);
    if (!ok) setErrorMsg(text || 'restart failed');
    setLocalOp(null);
  }

  async function handleRollback() {
    if (!c.service) return;
    setErrorMsg(null);
    setLocalOp('rolling_back');
    const { ok, text } = await doFetch(`/api/rollback/${encodeURIComponent(c.service)}`);
    if (!ok) setErrorMsg(text || 'rollback failed');
    setLocalOp(null);
  }

  return (
    <div className={cardClasses}>
      <div className="card-inner">
        <div className="card-left">
          <div className="service-row">
            <span className="service-name">{c.name}</span>
            <span className={`badge ${c.state === 'running' ? 'badge-green' : 'badge-blue'}`}>
              ● {c.state.toUpperCase()}
            </span>
          </div>
          <div className="meta">
            <div className="meta-row">
              <span className="lbl">image:</span>
              <span className="val">{c.image}</span>
            </div>
            <div className="meta-row">
              <span className="lbl">status:</span>
              <span className="val">{c.status}</span>
            </div>
            {c.prev_container && (
              <div className="meta-row">
                <span className="lbl">rollback&nbsp;target:</span>
                <span className="val">{c.prev_container}</span>
              </div>
            )}
          </div>
        </div>
        <div className="card-right">
          <div className={`status-msg${opLabel && !errorMsg ? ' spinning' : ''}${errorMsg ? ' err' : ''}`}>
            {!errorMsg && opLabel && <><span className="spin">⟳</span>{' '}{opLabel}</>}
            {errorMsg && <>✗ {errorMsg}</>}
          </div>
          <div className="btn-row">
            <button
              className={`btn ${inOp ? 'btn-in-progress' : 'btn-restart'}`}
              onClick={handleRestart}
              disabled={inOp}
            >
              {operation === 'restarting' ? '↺ Restarting…' : '↺ Restart'}
            </button>
            {c.rollback_available && (
              <button
                className={`btn ${inOp ? 'btn-in-progress' : 'btn-rollback'}`}
                onClick={handleRollback}
                disabled={inOp}
              >
                {operation === 'rolling_back' ? '↩ Rolling back…' : '↩ Rollback'}
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
