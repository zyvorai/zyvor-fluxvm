import { type FormEvent, type ReactNode, useState } from 'react';
import { ApiError, api, setToken } from './api';
import { Button } from './components';

/** Netra-style single-card sign-in: a bearer token, verified against the API before it is kept. */
export function LoginCard({ logo, product, blurb, onSignedIn, anonymousOk }: { logo: ReactNode; product: string; blurb: string; onSignedIn: () => void; anonymousOk?: boolean }) {
  const [token, setTok] = useState('');
  const [err, setErr] = useState('');
  const [busy, setBusy] = useState(false);
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr('');
    setToken(token.trim());
    try {
      await api.get('/v1/vms');
      onSignedIn();
    } catch (x) {
      setToken('');
      setErr(x instanceof ApiError && x.status === 401 ? 'That token was not accepted.' : (x as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <main className="fx-login">
      <div className="fx-hero-glow" aria-hidden />
      <div className="fx-login-logo">{logo}</div>
      <div className="fx-eyebrow">{product}</div>
      <h1>Sign in.</h1>
      <p className="fx-lede">{blurb}</p>
      <form className="fx-card fx-login-card" onSubmit={submit}>
        <label htmlFor="fx-token">API token</label>
        <input id="fx-token" type="password" autoComplete="off" autoFocus value={token} onChange={(e) => setTok(e.target.value)} placeholder="Bearer token from [[auth.tokens]]" />
        {err ? (
          <div className="fx-error" role="alert">
            {err}
          </div>
        ) : null}
        <Button variant="primary" size="lg" busy={busy} type="submit" disabled={!token.trim() && !anonymousOk}>
          Continue
        </Button>
        <small>The token stays in this tab’s session storage and is sent only to this daemon.</small>
      </form>
    </main>
  );
}
