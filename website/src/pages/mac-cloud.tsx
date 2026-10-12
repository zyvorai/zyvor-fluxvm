import type {ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import MacCloud from '@site/src/components/MacCloud';
import Reveal from '@site/src/components/Reveal';

import styles from './mac.module.css';

type UseCase = {icon: string; hue: string; title: string; body: string; to: string};

const USE_CASES: UseCase[] = [
  {
    icon: 'CI',
    hue: 'linear-gradient(135deg,#0a84ff,#64d2ff)',
    title: 'Xcode CI on macOS guests',
    body: 'Clone a prepared macOS template per job with APFS, run xcodebuild over SSH, throw it away. Two macOS guests per Mac, so add Macs to add lanes.',
    to: '/docs/macos#macos-guests',
  },
  {
    icon: 'AI',
    hue: 'linear-gradient(135deg,#5e5ce6,#bf5af2)',
    title: 'An agent farm',
    body: 'Disposable Linux sandboxes for coding agents through MCP: offline or allow-listed, forked into running copies in about 4 s, with a warm pool that restores in about 2 s.',
    to: '/agents',
  },
  {
    icon: 'OCI',
    hue: 'linear-gradient(135deg,#30d158,#64d2ff)',
    title: 'Containers, each in a VM',
    body: 'OCI images as lightweight VMs with a read-only root and uid 65534. linux/amd64 images run under Rosetta.',
    to: '/docs/oci-sandboxes',
  },
  {
    icon: 'Dev',
    hue: 'linear-gradient(135deg,#ff9f0a,#ff375f)',
    title: 'Dev boxes for the team',
    body: 'Describe a database, an app and a test runner in fluxvm.toml; fluxctl up --fleet picks the Mac that fits.',
    to: '/docs/macos-stacks',
  },
];

const STEPS: {n: number; title: string; body: string; code: string}[] = [
  {
    n: 1,
    title: 'Run FluxVM on every Mac',
    body: 'The daemon as a service in a logged-in session, so warm starts and snapshot restore survive a reboot.',
    code: ['brew install zyvorai/tap/fluxvm', 'brew services start zyvorai/tap/fluxvm'].join('\n'),
  },
  {
    n: 2,
    title: 'Start one central',
    body: 'The fleet registry and API. Built from source today (cargo build -p fluxvm-agent); it is not in the Homebrew package yet.',
    code: [
      'TOKEN=$(openssl rand -hex 16)',
      'fluxvm-agent central --listen 127.0.0.1:7799 --token $TOKEN \\',
      '  --public-url https://cloud.example.com',
    ].join('\n'),
  },
  {
    n: 3,
    title: 'Register each Mac',
    body: 'Each node reports free CPU and memory, its macOS guest count and what Virtualization.framework can do there.',
    code: [
      'fluxvm-agent node --name studio-1 \\',
      '  --central http://fleet-registry:7799 --token $TOKEN \\',
      '  --advertise-url http://studio-1.local:7788',
    ].join('\n'),
  },
  {
    n: 4,
    title: 'Give a team a tenant',
    body: 'A tenant gets its own token, quotas and Mac leases, and uses the fleet like one big machine.',
    code: [
      'fluxctl fleet --token $TOKEN tenant create acme --max-vms 8',
      'fluxctl fleet lease create          # rent a whole Mac, 24 h minimum',
      'fluxctl fleet launch --image debian-13 web-1',
      'fluxctl fleet ssh web-1             # through central; no route to the Mac',
    ].join('\n'),
  },
];

type Feature = {title: string; body: string; to: string; tag: string};
// Every item below was run against central plus simulated Macs (fluxvm-agent sim-node) and covered by unit tests;
// none has run across several real Macs. Sources: docs/mac-cloud-tenancy.md, docs/fleet-quickstart.md.
const FEATURES: Feature[] = [
  {
    title: 'Mac leases and tenants',
    body: 'A tenant rents a whole, empty Mac for at least 24 hours (Apple\u2019s licence rule), with quotas, scrub-on-release and drain.',
    to: '/docs/mac-cloud-tenancy',
    tag: 'Tested against simulated Macs',
  },
  {
    title: 'Snapshots and clones',
    body: 'Save a VM with or without memory, roll back while it runs, and clone it into a new VM under the same quota.',
    to: '/docs/mac-cloud-tenancy#snapshots-and-clones',
    tag: 'Tested against simulated Macs',
  },
  {
    title: 'Persistent volumes',
    body: 'Disks that outlive a VM. A VM that uses one is placed on its Mac, and two VMs never share one.',
    to: '/docs/mac-cloud-tenancy#volumes',
    tag: 'Tested against simulated Macs',
  },
  {
    title: 'SSH without exposing the Mac',
    body: 'fluxctl fleet ssh and port-forward go through central, so tenants never need a route to a Mac.',
    to: '/docs/mac-cloud-tenancy#ssh-and-port-tunnels',
    tag: 'Tested against simulated Macs',
  },
  {
    title: 'Warm pools',
    body: 'Booted VMs waiting, so a CI job starts in seconds; the pool boots a replacement after each claim.',
    to: '/docs/warm-pools',
    tag: 'Tested against simulated Macs',
  },
  {
    title: 'Graphs and Prometheus',
    body: 'CPU, memory and disk per VM and per Mac, fluxctl fleet top, and a /metrics endpoint with a scrape-only token.',
    to: '/docs/fleet-metrics',
    tag: 'Tested against simulated Macs',
  },
  {
    title: 'SSO and roles',
    body: 'Console sign-in through your OIDC provider, with viewer, member, admin and owner roles.',
    to: '/docs/mac-cloud-tenancy#sign-in-with-sso-and-team-members',
    tag: 'Implemented; no real identity provider run',
  },
  {
    title: 'Prices, statements, budgets',
    body: 'A price list, monthly statements per tenant (CSV too), budgets that warn at 80%, and signed webhooks.',
    to: '/docs/mac-cloud-billing',
    tag: 'Tested against simulated Macs',
  },
  {
    title: 'Go, Python, TypeScript, Terraform',
    body: 'Typed clients for the fleet API, an OpenAPI description and a Terraform provider.',
    to: '/docs/fleet-sdk',
    tag: 'Tested against fakes',
  },
];

function Hero() {
  return (
    <header className={styles.hero}>
      <div className="container">
        <div className={styles.pills}>
          <span className={styles.pill}>Mac mini</span>
          <span className={styles.pill}>Mac Studio</span>
          <span className={clsx(styles.pill, styles.pillBlue)}>vz fleet</span>
          <span className={styles.pill}>10 GbE &middot; Thunderbolt 5</span>
        </div>
        <Heading as="h1" className={styles.heroTitle}>
          Build a <span className={styles.gradientText}>Mac cloud</span>
          <br />
          from the Macs you have.
        </Heading>
        <p className={styles.heroSub}>
          A shelf of Mac minis and Mac Studios becomes one API: macOS guests for Xcode CI, Linux VMs,
          agent sandboxes and container VMs, placed on the Mac that fits.
        </p>
        <div className={styles.buttons}>
          <Link className="button button--primary button--lg" to="/docs/mac-cloud">
            Read the guide
          </Link>
          <Link className="button button--outline button--lg button--secondary" to="/docs/mac-cloud-operations">
            Run it for real
          </Link>
          <Link className="button button--outline button--lg button--secondary" to="/mac">
            FluxVM on one Mac
          </Link>
        </div>
        <MacCloud caption="Illustration. Fleet placement is verified on one real Mac plus simulated nodes; no second Mac has been used yet." />
      </div>
    </header>
  );
}

function UseCases() {
  return (
    <section className={clsx(styles.section, styles.alt)}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>What people build</p>
            <Heading as="h2" className={styles.h2}>
              One fleet, every kind of machine
            </Heading>
          </div>
          <div className={styles.grid4}>
            {USE_CASES.map((u) => (
              <div key={u.title} className={styles.perk}>
                <span className={styles.perkIcon} style={{background: u.hue}}>
                  {u.icon}
                </span>
                <Heading as="h3">
                  <Link to={u.to}>{u.title}</Link>
                </Heading>
                <p>{u.body}</p>
              </div>
            ))}
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function Features() {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>What your team gets</p>
            <Heading as="h2" className={styles.h2}>
              A cloud, not a shelf
            </Heading>
            <p className={styles.lead}>
              Tenants and leases, snapshots, volumes, tunnels, graphs and a console, in one API. Each card says what
              it has actually been run against.
            </p>
          </div>
          <div className={styles.grid3}>
            {FEATURES.map((f) => (
              <div key={f.title} className={styles.card}>
                <Heading as="h3">
                  <Link to={f.to}>{f.title}</Link>
                </Heading>
                <p>{f.body}</p>
                <span className={clsx(styles.tag, styles.tagWarn)}>{f.tag}</span>
              </div>
            ))}
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function Steps() {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>Four steps</p>
            <Heading as="h2" className={styles.h2}>
              From a shelf of Macs to a team&rsquo;s cloud
            </Heading>
          </div>
          <div className={styles.grid4}>
            {STEPS.map((s) => (
              <div key={s.n} className={styles.perk}>
                <span className={styles.perkIcon} style={{background: 'linear-gradient(135deg,#0a84ff,#5e5ce6)'}}>
                  {s.n}
                </span>
                <Heading as="h3">{s.title}</Heading>
                <p>{s.body}</p>
              </div>
            ))}
          </div>
          <div className={styles.terminal} style={{marginTop: '2rem'}}>
            <div className={styles.windowBar}>
              <span className={styles.dotRed} />
              <span className={styles.dotYellow} />
              <span className={styles.dotGreen} />
              <span className={styles.windowTitle}>Terminal &mdash; zsh</span>
            </div>
            <pre className={styles.terminalBody}>
              {STEPS.map((s, i) => (
                <div key={s.n}>
                  <span className={styles.prompt}># {s.n}. {s.title}</span>
                  {'\n'}
                  {s.code}
                  {i < STEPS.length - 1 && '\n\n'}
                </div>
              ))}
            </pre>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function Checklist() {
  return (
    <section className={clsx(styles.section, styles.alt)}>
      <div className="container">
        <Reveal>
          <div className="text--center">
            <p className={styles.kicker}>Before you rack it</p>
            <Heading as="h2" className={styles.h2}>
              A headless Mac, done right
            </Heading>
          </div>
          <div className={styles.grid3}>
            <div className={styles.card}>
              <Heading as="h3">Stay logged in</Heading>
              <p>
                Virtualization.framework restores saved state only in an unlocked session. Use a
                dedicated user with automatic login and the screen lock off.
              </p>
            </div>
            <div className={styles.card}>
              <Heading as="h3">One APFS volume</Heading>
              <p>
                Keep templates and VM state on the same volume so clones are instant. A clone across
                volumes is a full copy of a 20+ GB disk.
              </p>
            </div>
            <div className={styles.card}>
              <Heading as="h3">Size for unified memory</Heading>
              <p>
                FluxVM does not cap guest memory on a Mac. Budget macOS, your models and every guest
                together; the scheduler keeps headroom.
              </p>
            </div>
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
                Everything marked verified ran on <strong>one Apple M4 with macOS 27.2</strong>.
              </li>
              <li>
                <code>vz</code> placement in <code>fluxvm-agent central</code> was checked with one
                real Mac and simulated nodes. No multi-Mac fleet has run on real hardware yet.
              </li>
              <li>
                The platform features above were run against central plus simulated Macs and covered by tests; none
                has run across several real Macs, against real Stripe, or with a real identity provider.
              </li>
              <li>
                Self-service sign-up and Stripe are off by default. Sign-up confirms the address by a mailed link and
                needs a mail webhook before it can be opened; shared warm pools are off for tenants unless the
                operator allows them: see <Link to="/docs/mac-cloud-operations">the operations guide</Link>.
              </li>
              <li>Thunderbolt RDMA, the shared vmnet broker and physical USB passthrough are not verified.</li>
            </ul>
            <p style={{marginTop: '0.75rem', marginBottom: 0}}>
              <Link to="/docs/macos-cluster#verified-and-not-verified">The full list &rarr;</Link>
            </p>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

export default function MacCloudPage(): ReactNode {
  return (
    <Layout
      title="Build a Mac cloud"
      description="Turn Mac minis and Mac Studios into one VM API with FluxVM: macOS guests for Xcode CI, Linux VMs, agent sandboxes and container VMs on Apple's Virtualization.framework.">
      <Hero />
      <main>
        <UseCases />
        <Features />
        <Steps />
        <Checklist />
        <Honest />
      </main>
    </Layout>
  );
}
