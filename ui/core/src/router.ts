import { useEffect, useState } from 'react';

// Hash routing: works under any URL prefix a reverse proxy adds and needs no server-side fallback.
export interface Route {
  path: string;
  segs: string[];
  query: URLSearchParams;
}

export function parseHash(hash: string): Route {
  const h = hash.replace(/^#\/?/, '');
  const [p, q = ''] = h.split('?');
  const path = '/' + p.replace(/^\/+|\/+$/g, '');
  return { path: path === '/' ? '/' : path, segs: p.split('/').filter(Boolean), query: new URLSearchParams(q) };
}

export const navigate = (to: string): void => {
  location.hash = to.startsWith('#') ? to : '#' + to;
};

export function useRoute(): Route {
  const [r, setR] = useState(() => parseHash(location.hash));
  useEffect(() => {
    const f = () => setR(parseHash(location.hash));
    window.addEventListener('hashchange', f);
    return () => window.removeEventListener('hashchange', f);
  }, []);
  return r;
}
