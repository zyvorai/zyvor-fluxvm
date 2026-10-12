import type {ReactNode} from 'react';
import {useState} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import Reveal from '@site/src/components/Reveal';

import styles from './mac.module.css';

const REPO = 'https://github.com/zyvorai/zyvor-fluxvm';

// Every figure on this page was measured on one Apple M4 mini (16 GB, macOS 27.2) with a debian-13 guest.
// Sources: examples/agent-sandbox-demo/README.md (timings, two runs), docs/agent-density.md (memory, 21 sandboxes),
// docs/install-macos.md (install and MCP round trip, from the published v0.4.4 tap).
type Stat = {value: string; label: string; note: string};
const PROOF: Stat[] = [
  {value: '4.5 s', label: 'A fresh offline sandbox', note: 'cold boot, no network card; a default sandbox claiming a warm slot takes about 1 s'},
  {value: '~4 s', label: 'Fork into 4 running copies', note: 'they share the parent’s memory snapshot'},
  {value: '0.3 s', label: 'Four fixes tried and tested', note: 'in parallel, one in each copy'},
  {value: '31', label: 'MCP tools', note: 'create, run, fork, snapshot, delete; read-only unless you allow writes'},
];

const LOOP = [
  '$ examples/agent-sandbox-demo/run.sh',
  '1. offline sandbox',
  '   b1c1624a-d1bd-4be8-9542-68bfa337f12f ready in 4.4 s; network: lo UNKNOWN 127.0.0.1/8 ::1/128',
  '2. a project whose test fails',
  '   before the fix: FAILED (failures=1) | exit=1',
  '3. fork into four running copies',
  '   4 copies running in 4.0 s (parent snapshot 677 ms, slowest child 3303 ms)',
  '4. four different fixes, tried in parallel',
  '   all four tried and tested in 0.3 s',
  '   attempt-1: tests PASS',
  '   attempt-2: tests fail (rc=1)',
  '   attempt-3: tests fail (rc=1)',
  '   attempt-4: tests fail (rc=1)',
  '5. keep the one that passes',
  '   -    return sum(xs) / len(xs) + 1   # bug: an off-by-one nobody asked for',
  '   +    return sum(xs) / len(xs)',
  '   the other four are deleted',
].join('\n');

type Row = {profile: string; configured: string; shape: string; rss: string; n: number};
// Lowest and highest of every sample taken for that profile (1, 2 and 4 sandboxes at once); see docs/agent-density.md.
const MEMORY: Row[] = [
  {profile: 'tiny', configured: '512 MiB', shape: 'cold boot', rss: '347 to 779', n: 7},
  {profile: 'small', configured: '1 GiB', shape: 'cold boot', rss: '623 to 788', n: 7},
  {profile: 'standard', configured: '2 GiB', shape: 'claims a warm slot', rss: '205 to 712', n: 7},
];

const MCP_STEPS = [
  '# once per Mac',
  'brew install zyvorai/tap/fluxvm',
  'brew services start zyvorai/tap/fluxvm',
  '',
  '# once per agent client',
  'fluxctl mcp install claude-code --allow-write',
  '',
  '# then, in the agent: sandbox_create {"offline": true}',
  '#   sandbox_exec {"vm": id, "command": "pytest -q"}',
  '#   vm_fork {"vm": id, "count": 4}   ...   vm_delete',
].join('\n');

function Hero() {
  const [copied, setCopied] = useState(false);
  const cmd = 'brew install zyvorai/tap/fluxvm';
  const copy = () => {
    navigator.clipboard?.writeText(cmd).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };
  return (
    <header className={styles.hero}>
      <div className="container">
        <div className={styles.pills}>
          <span className={styles.pill}>Agent sandboxes</span>
          <span className={clsx(styles.pill, styles.pillBlue)}>Offline by default</span>
          <span className={styles.pill}>Fork in seconds</span>
          <span className={styles.pill}>MCP</span>
        </div>
        <Heading as="h1" className={styles.heroTitle}>
          A throwaway computer
          <br />
          <span className={styles.gradientText}>for every agent.</span>
        </Heading>
        <p className={styles.heroSub}>
          A real Linux VM for each task, on the Mac you already own. No network card unless you give it one.
          Fork it into four, try four ideas, keep the one that works, and throw the rest away.
        </p>
        <div className={styles.terminal} style={{maxWidth: 560, margin: '1.5rem auto 0'}}>
          <div className={styles.windowBar}>
            <span className={styles.dotRed} />
            <span className={styles.dotYellow} />
            <span className={styles.dotGreen} />
            <span className={styles.windowTitle}>Terminal &mdash; zsh</span>
            <button className={styles.copyBtn} onClick={copy} type="button">
              {copied ? 'Copied' : 'Copy'}
            </button>
          </div>
          <pre className={styles.terminalBody}>
            <span className={styles.prompt}>$ </span>
            {cmd}
          </pre>
        </div>
        <div className={styles.buttons} style={{marginTop: '1.5rem'}}>
          <Link className="button button--primary button--lg" to="/docs/install-macos">
            Install on a Mac
          </Link>
          <Link
            className="button button--outline button--lg button--secondary"
            href={`${REPO}/tree/main/examples/agent-sandbox-demo`}>
            Run the demo
          </Link>
        </div>
      </div>
    </header>
  );
}

function Proof() {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>Measured, not promised</p>
            <Heading as="h2" className={styles.h2}>
              Cheap enough to be disposable
            </Heading>
            <p className={styles.lead}>
              Seconds to a clean machine, and a copy of it for every idea. Every number below came from a real run
              on one Apple M4 mini.
            </p>
          </div>
          <div className={styles.stats}>
            {PROOF.map((s) => (
              <div key={s.label} className={styles.stat}>
                <span className={styles.statValue}>{s.value}</span>
                <span className={styles.statLabel}>{s.label}</span>
                <span className={styles.statNote}>{s.note}</span>
              </div>
            ))}
          </div>
          <p className={clsx(styles.footnote, 'text--center')}>
            <span className={clsx(styles.tag, styles.tagOk)}>Verified on M4</span> Two runs of the demo; the timings
            above are from both. Your Mac, disk and image will differ.
          </p>
        </Reveal>
      </div>
    </section>
  );
}

function Loop() {
  return (
    <section className={clsx(styles.section, styles.alt)}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>The loop</p>
            <Heading as="h2" className={styles.h2}>
              Four ideas. One survivor.
            </Heading>
            <p className={styles.lead}>
              An agent&rsquo;s test fails. It forks the sandbox into four running copies, tries a different fix in each,
              runs the tests in all four at once, and keeps whichever passes. This is the real output of{' '}
              <code>examples/agent-sandbox-demo/run.sh</code>.
            </p>
          </div>
          <div className={styles.terminal} style={{maxWidth: 860, margin: '0 auto'}}>
            <div className={styles.windowBar}>
              <span className={styles.dotRed} />
              <span className={styles.dotYellow} />
              <span className={styles.dotGreen} />
              <span className={styles.windowTitle}>agent-sandbox-demo &mdash; M4 mini</span>
            </div>
            <pre className={styles.terminalBody}>{LOOP}</pre>
          </div>
          <div className={styles.grid3} style={{marginTop: '2rem'}}>
            <div className={styles.card}>
              <Heading as="h3">Every attempt is isolated</Heading>
              <p>
                A broken <code>rm</code> or a half-applied patch happens in a copy that is deleted a second later.
                Nothing leaks into the next attempt or onto your Mac.
              </p>
            </div>
            <div className={styles.card}>
              <Heading as="h3">Parallel, not sequential</Heading>
              <p>
                The four copies run at the same time, so four attempts cost the time of one. The fork took about four
                seconds; the four test runs took 0.3.
              </p>
            </div>
            <div className={styles.card}>
              <Heading as="h3">Plain MCP tools</Heading>
              <p>
                <code>sandbox_create</code>, <code>sandbox_exec</code>, <code>vm_fork</code>, <code>vm_delete</code>.
                The script is those same calls over REST; a model drives them the same way.
              </p>
            </div>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function Offline() {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>Private by construction</p>
            <Heading as="h2" className={styles.h2}>
              No network card. Still smart.
            </Heading>
            <p className={styles.lead}>
              An offline sandbox has no network device at all: only a loopback. Commands and files still work over
              vsock. When the agent needs a model, one allow-listed port relays to a server on the Mac, so the code
              and the prompts never leave the machine.
            </p>
          </div>
          <div className={styles.grid3}>
            <div className={styles.card}>
              <Heading as="h3">Offline</Heading>
              <p>
                <code>sandbox_create {'{'}"offline": true{'}'}</code>. The guest can reach nothing: <code>ip -br a</code>{' '}
                shows <code>lo</code> only.
              </p>
              <span className={clsx(styles.tag, styles.tagOk)}>Verified on M4</span>
            </div>
            <div className={styles.card}>
              <Heading as="h3">Allow-listed</Heading>
              <p>
                <code>allow_hosts</code> opens ports 80 and 443 to the names you list, through a proxy on the host
                that refuses everything else.
              </p>
              <span className={clsx(styles.tag, styles.tagWarn)}>Implemented, not yet hardware-verified</span>
            </div>
            <div className={styles.card}>
              <Heading as="h3">A local model, one port</Heading>
              <p>
                A guest with no network card reached a real <code>llama-server</code> model on the Mac over one vsock
                port, and still could not reach the internet.
              </p>
              <span className={clsx(styles.tag, styles.tagOk)}>Verified on M4, small model</span>
            </div>
          </div>
          <p className={clsx(styles.footnote, 'text--center')}>
            The model run used a deliberately tiny 135M-parameter model, so its answers are weak: it proves the path,
            not answer quality. MLX and larger models were not run.
          </p>
        </Reveal>
      </div>
    </section>
  );
}

function Cost() {
  return (
    <section className={clsx(styles.section, styles.alt)}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>What it costs</p>
            <Heading as="h2" className={styles.h2}>
              Memory you can plan with
            </Heading>
            <p className={styles.lead}>
              Resident size per sandbox as macOS reports it, on a 16 GB M4 mini, idle. It varies a lot, so we show the lowest
              and the highest value seen, not a single number.
            </p>
          </div>
          <div className={styles.tableWrap}>
            <table className={styles.macTable}>
              <thead>
                <tr>
                  <th>Profile</th>
                  <th>Configured</th>
                  <th>Start</th>
                  <th>Sandboxes sampled</th>
                  <th>Resident MiB, lowest to highest</th>
                </tr>
              </thead>
              <tbody>
                {MEMORY.map((r) => (
                  <tr key={r.profile}>
                    <td>
                      <code>{r.profile}</code>
                    </td>
                    <td>{r.configured}</td>
                    <td>{r.shape}</td>
                    <td>{r.n}</td>
                    <td>{r.rss}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className={clsx(styles.footnote, 'text--center')}>
            Plan on about <strong>800 MiB per active sandbox</strong>: the lowest value is a floor, not a plan. On a
            16 GB Mac that leaves room for roughly a dozen active sandboxes next to macOS and your apps; that last line
            is an estimate, not a measurement. Idle and hibernated sandboxes cost less. Measured: 21 sandboxes. Not
            measured: guests doing real work, eight or more at once, Mac Studio sizes.{' '}
            <Link to="/docs/agent-density#what-a-sandbox-costs-on-a-real-mac">The full table</Link>
          </p>
        </Reveal>
      </div>
    </section>
  );
}

function PlugIn() {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>Plugs into your agent</p>
            <Heading as="h2" className={styles.h2}>
              Two commands from a clean Mac
            </Heading>
            <p className={styles.lead}>
              Claude Code, Cursor, Claude Desktop, Codex, VS Code, Windsurf and Gemini CLI each get a{' '}
              <code>fluxvm</code> MCP server in their own config. Tested end to end from a fresh home directory on the
              published package: handshake, 31 tools, a sandbox created, a command run, a fork, a delete.
            </p>
          </div>
          <div className={styles.terminal} style={{maxWidth: 760, margin: '0 auto'}}>
            <div className={styles.windowBar}>
              <span className={styles.dotRed} />
              <span className={styles.dotYellow} />
              <span className={styles.dotGreen} />
              <span className={styles.windowTitle}>Terminal &mdash; zsh</span>
            </div>
            <pre className={styles.terminalBody}>{MCP_STEPS}</pre>
          </div>
          <p className={clsx(styles.footnote, 'text--center')}>
            Write tools (<code>--allow-write</code>) can create, run commands in and delete VMs; leave them off for a
            read-only agent. <Link to="/docs/mcp">MCP reference</Link>
          </p>
        </Reveal>
      </div>
    </section>
  );
}

function Honest(): ReactNode {
  return (
    <section className={clsx(styles.section, styles.alt)}>
      <div className="container">
        <Reveal>
          <div className={styles.honest}>
            <Heading as="h3">What to know before you rely on it</Heading>
            <ul>
              <li>
                <strong>Only an offline (or no-NAT) sandbox can fork.</strong> A default sandbox sits on the
                Mac&rsquo;s NAT, and its copies would share its address, so the daemon refuses.
              </li>
              <li>
                <strong>Forked copies keep the parent&rsquo;s identity</strong> (hostname, machine id, SSH host
                keys): an offline sandbox has no guest agent over SSH for the reset. Fine for throwaway attempts; not
                for anything that registers itself somewhere.
              </li>
              <li>
                <strong>The first sandbox on a fresh daemon took 61 s</strong> while the image was prepared; later
                ones took about 4.5 s, and about 1 s when claiming a warm slot.
              </li>
              <li>
                This page is about Linux guests. macOS guests are a different, less proven workflow: see{' '}
                <Link to="/mac">FluxVM on Mac</Link>.
              </li>
              <li>
                Everything here ran on <strong>one Apple M4 with macOS 27.2</strong>. The package is ad hoc signed;
                vmnet, bridged networking and USB passthrough need a Developer ID build.
              </li>
            </ul>
            <p style={{marginTop: '0.75rem', marginBottom: 0}}>
              <Link to="/docs/macos#what-is-verified">The full list of what is verified &rarr;</Link>
            </p>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function Cta() {
  return (
    <section className={styles.cta}>
      <div className="container">
        <Reveal>
          <Heading as="h2" className={styles.h2}>
            Give your agent a clean machine.
          </Heading>
          <p className={styles.ctaSub}>
            <code>brew install zyvorai/tap/fluxvm</code>, then run the demo and watch four attempts become one.
          </p>
          <div className={styles.buttons}>
            <Link className="button button--primary button--lg" to="/docs/install-macos">
              Install on a Mac
            </Link>
            <Link className="button button--outline button--lg button--secondary" to="/docs/macos-sandboxes">
              Sandbox guide
            </Link>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

export default function AgentsPage(): ReactNode {
  return (
    <Layout
      title="Agent sandboxes on your Mac"
      description="A throwaway Linux VM for every AI agent task on Apple silicon: offline by default, forked into running copies in seconds, with a local model and no network card. MCP for Claude Code, Cursor, Codex and more.">
      <Hero />
      <main>
        <Proof />
        <Loop />
        <Offline />
        <Cost />
        <PlugIn />
        <Honest />
        <Cta />
      </main>
    </Layout>
  );
}
