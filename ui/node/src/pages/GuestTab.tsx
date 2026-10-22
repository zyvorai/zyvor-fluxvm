import { useRef, useState } from 'react';
import { Download, Play, Upload } from 'lucide-react';
import { Button, Card, Field, Pill, api } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { type ExecResult, MAX_UPLOAD_BYTES, baseName, download, execFailure, execTone, fromBase64, isAbsPath, toBase64 } from '../lib/exec';

/** Run commands and move files through the guest agent. */
export function GuestTab({ id, running, notify }: { id: string; running: boolean; notify: Notify }) {
  const [cmd, setCmd] = useState('uname -a');
  const [timeout, setTimeoutS] = useState('30');
  const [res, setRes] = useState<ExecResult | null>(null);
  const [busy, setBusy] = useState('');
  const [err, setErr] = useState('');
  const [path, setPath] = useState('/etc/os-release');
  const [dest, setDest] = useState('/tmp/');
  const pick = useRef<HTMLInputElement>(null);

  const ping = async () => {
    setBusy('ping');
    try {
      await api.post(`/v1/vms/${id}/agent/ping`);
      notify('The guest agent answered');
    } catch (e) {
      notify(`The guest agent did not answer: ${(e as Error).message}`, 'bad');
    } finally {
      setBusy('');
    }
  };
  const run = async () => {
    setBusy('exec');
    setErr('');
    setRes(null);
    try {
      const r = await api.post<ExecResult>(`/v1/vms/${id}/agent`, { command: cmd, timeout_seconds: Number(timeout) || 30 });
      const bad = execFailure(r);
      if (bad) setErr(bad);
      else setRes(r);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy('');
    }
  };
  const get = async () => {
    if (!isAbsPath(path)) return notify('Enter an absolute path.', 'bad');
    setBusy('get');
    try {
      const r = await api.post<{ content_base64: string }>(`/v1/vms/${id}/agent/get-file`, { path });
      download(baseName(path), fromBase64(r.content_base64));
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const put = async (file: File) => {
    const target = dest.endsWith('/') ? dest + file.name : dest;
    if (!isAbsPath(target)) return notify('Enter an absolute destination path.', 'bad');
    if (file.size > MAX_UPLOAD_BYTES) return notify(`Files over ${MAX_UPLOAD_BYTES / 1048576} MiB are not supported here.`, 'bad');
    setBusy('put');
    try {
      await api.post(`/v1/vms/${id}/agent/put-file`, { path: target, content_base64: toBase64(new Uint8Array(await file.arrayBuffer())) });
      notify(`Uploaded to ${target}`);
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
      if (pick.current) pick.current.value = '';
    }
  };

  if (!running) return <Card title="Guest"><p className="fx-muted">Start the machine to run commands and transfer files through its agent.</p></Card>;
  return (
    <div className="fx-grid cols-2">
      <Card title="Run a command" action={<Button size="sm" busy={busy === 'ping'} onClick={ping}>Ping agent</Button>}>
        <Field label="Command">{(i) => <input id={i} className="fx-input fx-mono" value={cmd} onChange={(e) => setCmd(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && !busy && run()} />}</Field>
        <Field label="Timeout (seconds)">{(i) => <input id={i} className="fx-input" inputMode="numeric" value={timeout} onChange={(e) => setTimeoutS(e.target.value.replace(/\D/g, ''))} />}</Field>
        <Button variant="primary" busy={busy === 'exec'} disabled={!cmd.trim()} onClick={run}><Play size={14} />Run</Button>
        {err ? <div className="fx-error" role="alert" style={{ marginTop: 12 }}>{err}</div> : null}
        {res ? (
          <div style={{ marginTop: 16 }}>
            <Pill tone={execTone(res)}>exit {res.exit_code ?? '?'}</Pill>
            {res.stdout ? <pre className="fx-code" style={{ marginTop: 10 }}>{res.stdout}</pre> : null}
            {res.stderr ? <pre className="fx-code fx-code-err" style={{ marginTop: 10 }}>{res.stderr}</pre> : null}
          </div>
        ) : null}
      </Card>
      <Card title="Files">
        <Field label="Download from the guest">{(i) => <input id={i} className="fx-input fx-mono" value={path} onChange={(e) => setPath(e.target.value)} />}</Field>
        <Button busy={busy === 'get'} onClick={get}><Download size={14} />Download</Button>
        <h3 className="fx-subhead">Upload to the guest</h3>
        <Field label="Destination" hint="End with / to keep the file's name.">{(i) => <input id={i} className="fx-input fx-mono" value={dest} onChange={(e) => setDest(e.target.value)} />}</Field>
        <input ref={pick} type="file" hidden onChange={(e) => e.target.files?.[0] && put(e.target.files[0])} />
        <Button busy={busy === 'put'} onClick={() => pick.current?.click()}><Upload size={14} />Choose a file…</Button>
      </Card>
    </div>
  );
}
