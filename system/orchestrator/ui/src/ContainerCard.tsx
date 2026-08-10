import { useState } from 'react';
import { Container } from './types';

interface Props {
  container: Container;
}

const OP_LABEL: Record<string, string> = {
  rolling_back: 'Rolling back…',
  stopping:     'Stopping…',
  starting:     'Starting…',
  killing:      'Killing…',
  restarting:   'Restarting…',
};

export function ContainerCard({ container: c }: Props) {
  const [localOp,  setLocalOp]  = useState<string | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const operation = c.operation ?? localOp;
  const inOp      = Boolean(operation);

  const cardClasses = [
    'card',
    c.state !== 'running' ? 'inactive'    : '',
    inOp                  ? 'in-progress' : '',
    errorMsg              ? 'error'       : '',
  ].filter(Boolean).join(' ');

  const opLabel = operation ? (OP_LABEL[operation] ?? null) : null;

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

  async function run(op: string, url: string, failMsg: string) {
    setErrorMsg(null);
    setLocalOp(op);
    const { ok, text } = await doFetch(url);
    if (!ok) setErrorMsg(text || failMsg);
    setLocalOp(null);
  }

  const stop     = () => run('stopping',     `/api/stop/${encodeURIComponent(c.name)}`,     'stop failed');
  const start    = () => run('starting',     `/api/start/${encodeURIComponent(c.name)}`,    'start failed');
  const kill     = () => run('killing',      `/api/kill/${encodeURIComponent(c.name)}`,     'kill failed');
  const restart  = () => run('restarting',   `/api/restart/${encodeURIComponent(c.name)}`,  'restart failed');
  const rollback = () => { if (c.service) run('rolling_back', `/api/rollback/${encodeURIComponent(c.service)}`, 'rollback failed'); };

  const isRunning = c.state === 'running';
  const stateLabel = isRunning
    ? 'Running'
    : c.state ? c.state[0].toUpperCase() + c.state.slice(1) : 'Off';

  return (
    <div className={cardClasses} data-od-id={`card-${c.name}`}>
      <div className="card-head">
        <div className="card-id">
          <span className={`status-dot${isRunning ? ' on' : ''}`} />
          <span className="card-name">{c.name}</span>
          <span className={`status-label${isRunning ? ' on' : ''}`}>{stateLabel}</span>
          {c.serving && <span className="serving-tag">Serving</span>}
        </div>
        <div className="card-actions">
          {isRunning ? (
            <>
              <button
                className={`btn ${operation === 'stopping' ? 'active' : ''}`}
                onClick={stop}
                disabled={inOp}
              >
                {operation === 'stopping' ? 'Stopping…' : 'Stop'}
              </button>
              <button
                className={`btn danger ${operation === 'killing' ? 'active' : ''}`}
                onClick={kill}
                disabled={operation === 'killing'}
              >
                {operation === 'killing' ? 'Killing…' : 'Kill'}
              </button>
              <button
                className={`btn ${operation === 'restarting' ? 'active' : ''}`}
                onClick={restart}
                disabled={inOp}
              >
                {operation === 'restarting' ? 'Restarting…' : 'Restart'}
              </button>
            </>
          ) : (
            <button
              className={`btn ${operation === 'starting' ? 'active' : ''}`}
              onClick={start}
              disabled={inOp}
            >
              {operation === 'starting' ? 'Starting…' : 'Start'}
            </button>
          )}
          {c.rollback_available && (
            <button
              className={`btn text ${operation === 'rolling_back' ? 'active' : ''}`}
              onClick={rollback}
              disabled={inOp}
              title="Restarts the stopped peer container, runs health checks, flips Caddy routing to it"
            >
              {operation === 'rolling_back' ? 'Rolling back…' : 'Rollback'}
            </button>
          )}
        </div>
      </div>
      {(opLabel || errorMsg) && (
        <div className={`status-msg${opLabel && !errorMsg ? ' spin' : ''}${errorMsg ? ' err' : ''}`}>
          {!errorMsg && opLabel && <><span className="spinner" />{opLabel}</>}
          {errorMsg && <>Error: {errorMsg}</>}
        </div>
      )}
      <div className="card-meta">
        <div className="meta-line">
          <span className="lbl">image</span>
          <span className="val">{c.image}</span>
        </div>
        <div className="meta-line">
          <span className="lbl">status</span>
          <span className="val">{c.status}</span>
        </div>
        {c.prev_container && (
          <div className="meta-line">
            <span className="lbl">rollback target</span>
            <span className="val">{c.prev_container}</span>
          </div>
        )}
      </div>
    </div>
  );
}
