import { describe, expect, it } from 'vitest';
import { type DockerContainer, actionDone, containerTone, filterContainers, fmtSize, portsText, summarize } from './docker';

const c = (name: string, state: string, status: string, image = 'nginx'): DockerContainer => ({ id: name + '0000', name, image, command: '', state, status, created: 0, ports: [] });

describe('container tone', () => {
  it('reads the state and the exit code', () => {
    expect(containerTone(c('a', 'running', 'Up 2 minutes'))).toBe('ok');
    expect(containerTone(c('a', 'running', 'Up 2 minutes (unhealthy)'))).toBe('bad');
    expect(containerTone(c('a', 'running', 'Up 5 seconds (health: starting)'))).toBe('warn');
    expect(containerTone(c('a', 'restarting', 'Restarting (1) 3 seconds ago'))).toBe('warn');
    expect(containerTone(c('a', 'exited', 'Exited (0) 3 hours ago'))).toBe('idle');
    expect(containerTone(c('a', 'exited', 'Exited (137) 3 hours ago'))).toBe('bad');
    expect(containerTone(c('a', 'created', 'Created'))).toBe('info');
    expect(containerTone(c('a', 'dead', ''))).toBe('bad');
  });
  it('counts running, stopped and failing apart', () => {
    const s = summarize([c('a', 'running', 'Up 1h'), c('b', 'exited', 'Exited (0) 1h ago'), c('c', 'exited', 'Exited (1) 1h ago'), c('d', 'running', 'Up 1h (unhealthy)')]);
    expect(s).toEqual({ running: 1, stopped: 1, failing: 2 });
  });
});

describe('ports and sizes', () => {
  it('writes published and exposed ports once each', () => {
    expect(portsText([{ private: 80, public: 8080, type: 'tcp', ip: '0.0.0.0' }, { private: 80, public: 8080, type: 'tcp', ip: '::' }, { private: 443, public: null, type: 'tcp', ip: null }])).toBe('8080→80/tcp, 443/tcp');
    expect(portsText([])).toBe('');
  });
  it('sizes like docker images', () => {
    expect([fmtSize(0), fmtSize(999), fmtSize(1500), fmtSize(142_000_000), fmtSize(2_300_000_000), fmtSize(-1)]).toEqual(['0 B', '999 B', '1.5 kB', '142 MB', '2.3 GB', '—']);
  });
});

describe('filter', () => {
  const list = [c('web', 'running', 'Up 1h', 'nginx:1'), c('db', 'exited', 'Exited (0) 1h ago', 'postgres:17')];
  it('matches every word, in any field', () => {
    expect(filterContainers(list, 'postgres', 'all').map((x) => x.name)).toEqual(['db']);
    expect(filterContainers(list, 'nginx up', 'all').map((x) => x.name)).toEqual(['web']);
    expect(filterContainers(list, 'nginx postgres', 'all')).toEqual([]);
  });
  it('limits to a group', () => {
    expect(filterContainers(list, '', 'running').map((x) => x.name)).toEqual(['web']);
    expect(filterContainers(list, '', 'stopped').map((x) => x.name)).toEqual(['db']);
  });
});

describe('actionDone', () => {
  it('says what happened, and when nothing had to', () => {
    expect(actionDone('web', 'start', true)).toBe('web started');
    expect(actionDone('web', 'stop', true)).toBe('web stopped');
    expect(actionDone('web', 'restart', true)).toBe('web restarted');
    expect(actionDone('web', 'start', false)).toBe('web was already running');
    expect(actionDone('web', 'stop', false)).toBe('web was already stopped');
  });
});
