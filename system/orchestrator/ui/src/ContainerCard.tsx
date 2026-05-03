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

  async function handleRestart() {
    setErrorMsg(null);
    setLocalOp('restarting');
    try {
      const resp = await fetch(`/api/restart/${encodeURIComponent(c.name)}`, { method: 'POST' });
      if (!resp.ok) setErrorMsg((await resp.text()) || 'restart failed');
    } catch {
      setErrorMsg('network error');
    } finally {
      setLocalOp(null);
    }
  }

  async function handleRollback() {
    if (!c.service) return;
    setErrorMsg(null);
    setLocalOp('rolling_back');
    try {
      const resp = await fetch(`/api/rollback/${encodeURIComponent(c.service)}`, { method: 'POST' });
      if (!resp.ok) setErrorMsg((await resp.text()) || 'rollback failed');
    } catch {
      setErrorMsg('network error');
    } finally {
      setLocalOp(null);
    }
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
