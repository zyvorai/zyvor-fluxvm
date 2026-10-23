import type {ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import Reveal from '@site/src/components/Reveal';

import styles from './mac.module.css';

type Area = {title: string; body: string; to: string; tag: string};

// Status tags say what each area has actually been run against. The node pages have only run against a mock API. The fleet VM tools
// (screen, snapshots, clones, commands) and forms have also run against a real central: see docs/ui.md, "VM tools on a real guest".
const NODE: Area[] = [
  {title: 'Machines', body: 'A create wizard, then per-machine console, guest exec and files, network and egress policy, storage, snapshots and backups, and migration. VMs that share a stack get Start all and Stop all, and published ports show in the list.', to: '/docs/ui', tag: 'Built; run against a mock API'},
  {title: 'Sandboxes and agents', body: 'Disposable sandboxes with their warm pool, and agent sessions with approvals, checkpoints and a terminal.', to: '/docs/agent-sessions', tag: 'Built; run against a mock API'},
  {title: 'Network policy', body: 'Decide what a user-mode guest may reach: presets, observe then suggest then apply, and the node default.', to: '/docs/egress-policy', tag: 'Built; run against a mock API'},
  {title: 'Images and pools', body: 'The checksum-pinned catalog, machine templates, warm pools and OCI images, each with a copy-as-fluxctl button.', to: '/docs/ui', tag: 'Built; run against a mock API'},
  {title: 'Host', body: 'Memory, pressure and topology, GPU passthrough on Linux, and the Apple host on a Mac. Pages a host cannot serve are hidden.', to: '/docs/ui', tag: 'Built; run against a mock API'},
  {title: 'Access and audit', body: 'Your quota, the event history and an API explorer that lists every route and runs the safe reads.', to: '/docs/ui', tag: 'Built; run against a mock API'},
];

const FLEET: Area[] = [
  {title: 'Hosts', body: 'Cordon, drain, scrub and lease hosts, with the 24-hour minimum lease shown as it elapses.', to: '/docs/fleet-quickstart', tag: 'Built; run against a mock API'},
  {title: 'Virtual machines', body: 'Launch from the image catalog, power, logs and delete, across every host a tenant can see.', to: '/docs/fleet-quickstart', tag: 'Built; run against a mock API'},
  {title: 'A machine\'s screen, snapshots and commands', body: 'Open a VM for its live screen (and, for macOS guests, Control: clicks, scrolling and keys), snapshots, clones and a command runner.', to: '/docs/ui', tag: 'Screen, snapshots, clones and commands run against a real Linux guest through a real central; Control only against a mock'},
  {title: 'Tenants, keys and members', body: 'Quotas, token rotation, named API keys that are shown once, and a tenant\'s members: add by work email, change a role, remove.', to: '/docs/mac-cloud-tenancy', tag: 'Built; run against a mock API'},
  {title: 'Image catalog', body: 'Operators add, edit and remove catalog images; tenants launch from them.', to: '/docs/ui', tag: 'Built; run against a mock API'},
  {title: 'Billing and activity', body: 'Usage and billing, the audit log, alerts and signed webhooks. Sign in with a token or with SSO.', to: '/docs/mac-cloud-billing', tag: 'Built; run against a mock API'},
];

function Hero() {
  return (
    <header className={styles.hero}>
      <div className="container">
        <div className={styles.pills}>
          <span className={styles.pill}>In the binary</span>
          <span className={clsx(styles.pill, styles.pillBlue)}>Dark and light</span>
          <span className={styles.pill}>&#8984;K</span>
        </div>
        <Heading as="h1" className={styles.heroTitle}>
          FluxVM <span className={styles.gradientText}>Studio</span>
          <br />
          every feature, one calm place.
        </Heading>
        <p className={styles.heroSub}>
          A web UI for the node daemon and for fleet central, embedded in the binary. No install, no Node at runtime and
          no extra port: run <code>fluxctl dashboard</code>.
        </p>
        <div className={styles.buttons}>
          <Link className="button button--primary button--lg" to="/docs/ui">
            Read the guide
          </Link>
          <Link className="button button--outline button--lg button--secondary" to="/docs/install">
            Install FluxVM
          </Link>
        </div>
      </div>
    </header>
  );
}

function Areas({kicker, title, areas, alt}: {kicker: string; title: string; areas: Area[]; alt?: boolean}) {
  return (
    <section className={clsx(styles.section, alt && styles.alt)}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>{kicker}</p>
            <Heading as="h2" className={styles.h2}>
              {title}
            </Heading>
          </div>
          <div className={styles.grid3}>
            {areas.map((a) => (
              <div key={a.title} className={styles.card}>
                <Heading as="h3">
                  <Link to={a.to}>{a.title}</Link>
                </Heading>
                <p>{a.body}</p>
                <span className={clsx(styles.tag, styles.tagWarn)}>{a.tag}</span>
              </div>
            ))}
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function Honest(): ReactNode {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <div className={styles.honest}>
            <Heading as="h3">What is and is not verified</Heading>
            <ul>
              <li>
                Typechecking, unit tests and a headless-Chrome pass over every page (phone and desktop width, both
                themes) run against a <strong>mock API</strong>. The mock serves plausible shapes, not real daemon output.
              </li>
              <li>
                <strong>Not yet run against a live daemon:</strong> the in-browser terminals, the strict content
                security policy with the lazily loaded terminal chunk, and several response shapes that are rendered
                generically for now.
              </li>
              <li>
                Through a Kairon node the UI is reached on the node&rsquo;s console relay, which must stay off the
                internet. See <Link to="/docs/ui">the guide</Link>.
              </li>
            </ul>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

export default function Studio(): ReactNode {
  return (
    <Layout title="FluxVM Studio" description="The FluxVM web UI for the node daemon and fleet central, embedded in the binary.">
      <Hero />
      <main>
        <Areas kicker="On a node" title="Everything the daemon does" areas={NODE} />
        <Areas kicker="On fleet central" title="Hosts, tenants and bills" areas={FLEET} alt />
        <Honest />
      </main>
    </Layout>
  );
}
