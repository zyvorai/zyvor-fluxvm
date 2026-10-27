import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import Reveal from '@site/src/components/Reveal';

import styles from './kubernetes.module.css';

const REPO = 'https://github.com/zyvorai/zyvor-fluxvm';

type Stat = {value: string; label: string; note: string};

// Measured on a real k3s cluster on a Mac mini (2 vCPU, 3 GiB). Sources: docs/kubernetes-on-mac.md, "Time travel for
// your cluster", "Sleep and wake" and "A project in one command".
const STATS: Stat[] = [
  {value: '1.5 s', label: 'Save the whole cluster', note: 'memory, disk and every pod'},
  {value: '12.5 s', label: 'Undo a bad apply', note: 'kube try, then kube undo'},
  {value: '2 s', label: 'Wake from sleep', note: 'sleeping took 6 s'},
  {value: '7 s', label: 'Start again', note: 'kube start on a project that is already up'},
];

type Feature = {title: string; body: string; cmd: string};

const FEATURES: Feature[] = [
  {
    title: 'Plain kubectl',
    body: 'The cluster is merged into your kubeconfig. No port-forward, no VM to log into.',
    cmd: 'kubectl get pods',
  },
  {
    title: 'Real names',
    body: 'Every Service and Ingress host answers on a .localhost name, with HTTPS from a local CA limited to .localhost.',
    cmd: 'https://web.localhost',
  },
  {
    title: 'Time machine',
    body: 'A checkpoint is the whole running machine. Undo a kubectl apply, including what it did to running pods.',
    cmd: 'kube try -- kubectl apply -f x.yaml',
  },
  {
    title: 'Services in one line',
    body: 'Seventeen development services with their usual ports on the Mac and a connection string.',
    cmd: 'kube add postgres',
  },
  {
    title: 'Save a file, see it run',
    body: 'Rebuild an image on save, or copy files into a pod with no build at all.',
    cmd: 'kube sync web --to /app',
  },
  {
    title: 'Every replica, one stream',
    body: 'Logs from all pods and containers behind a workload, each line tagged and colored by pod.',
    cmd: 'kube logs web -f',
  },
  {
    title: 'Copy files both ways',
    body: 'Like docker cp. Files travel as a tar stream, nothing to install in the pod.',
    cmd: 'kube cp ./site web:/usr/share/nginx/html',
  },
  {
    title: 'Backups that are a file',
    body: 'Postgres, MySQL and MongoDB contents as one private file on your Mac, and back again.',
    cmd: 'kube db-backup postgres',
  },
  {
    title: 'It tells you what is wrong',
    body: 'A doctor for the Mac and the VM, and a macOS notification when a pod crash-loops.',
    cmd: 'kube doctor',
  },
  {
    title: 'Sleeps when you do',
    body: 'Park the cluster to a checkpoint when idle or on battery. The next kubectl call wakes it.',
    cmd: 'kube supervise --idle-sleep 30m',
  },
  {
    title: 'The same bytes everywhere',
    body: 'Print every running image as a tag plus digest and paste it into a manifest.',
    cmd: 'kube pin',
  },
  {
    title: 'In the menu bar',
    body: 'Pod count, failing pods and one-click Sleep and Wake, through SwiftBar or xbar.',
    cmd: 'kube menubar',
  },
];

const APPS = [
  'Postgres', 'Redis', 'MySQL', 'MongoDB', 'ClickHouse', 'Redpanda', 'NATS', 'RabbitMQ', 'Mailpit',
  'Grafana', 'Qdrant', 'Meilisearch', 'Keycloak', 'SeaweedFS', 'Vault', 'Jaeger', 'Temporal',
];

function Hero() {
  return (
    <header className={styles.hero}>
      <div className="container">
        <p className={styles.eyebrow}>FluxVM for Mac</p>
        <Heading as="h1" className={styles.heroTitle}>
          Kubernetes, on your Mac.
          <br />
          <span className={styles.gradientText}>Without the Linux box.</span>
        </Heading>
        <p className={styles.heroSub}>
          A real k3s cluster in a virtual machine that Apple silicon runs natively. It behaves like the cluster you
          have on Linux, and it can do things a Linux cluster cannot, because saving it saves the whole machine.
        </p>
        <div className={styles.buttons}>
          <Link className={styles.primary} to="/docs/kubernetes-on-mac">
            Read the guide
          </Link>
          <Link className={styles.secondary} to="/docs/install-macos">
            Install
          </Link>
        </div>
        <pre className={styles.terminal} aria-label="Starting a project">
          <code>
            <span className={styles.dim}>$ </span>fluxctl kube start{'\n'}
            <span className={styles.ok}>cluster shop is up</span>
            {'\n'}
            <span className={styles.ok}>added postgres</span>
            {'  '}postgresql://postgres:dev@127.0.0.1:5432/postgres{'\n'}
            <span className={styles.ok}>applied</span>
            {'      '}k8s/web.yaml{'\n'}
            <span className={styles.ok}>forwarded</span>
            {'    '}http://web.localhost{'\n'}
            <span className={styles.dim}>4 min 26 s from nothing, 7 s the second time</span>
          </code>
        </pre>
      </div>
    </header>
  );
}

function Stats() {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <p className={styles.kicker}>One thing Linux cannot do</p>
          <Heading as="h2" className={styles.h2}>
            A checkpoint is the whole running machine.
          </Heading>
          <p className={styles.lead}>
            Apple's Virtualization framework saves memory and device state, and the disk is cloned with APFS. Saving
            costs about a second and next to nothing until the disk changes.
          </p>
        </Reveal>
        <div className={styles.stats}>
          {STATS.map((s, i) => (
            <Reveal key={s.label} delay={i * 80}>
              <div className={styles.stat}>
                <span className={styles.statValue}>{s.value}</span>
                <span className={styles.statLabel}>{s.label}</span>
                <span className={styles.statNote}>{s.note}</span>
              </div>
            </Reveal>
          ))}
        </div>
        <p className={styles.footnote}>
          Measured on a real k3s cluster (2 vCPU, 3 GiB) on a Mac mini. Checkpoints work on single-node clusters.
        </p>
      </div>
    </section>
  );
}

function Features() {
  return (
    <section className={`${styles.section} ${styles.alt}`}>
      <div className="container">
        <Reveal>
          <p className={styles.kicker}>Everything a Linux developer expects</p>
          <Heading as="h2" className={styles.h2}>
            It feels local, because it is.
          </Heading>
        </Reveal>
        <div className={styles.grid}>
          {FEATURES.map((f, i) => (
            <Reveal key={f.title} delay={(i % 3) * 80}>
              <div className={styles.card}>
                <h3>{f.title}</h3>
                <p>{f.body}</p>
                <code className={styles.cmd}>{f.cmd}</code>
              </div>
            </Reveal>
          ))}
        </div>
      </div>
    </section>
  );
}

function Apps() {
  return (
    <section className={styles.section}>
      <div className="container">
        <Reveal>
          <p className={styles.kicker}>kube add</p>
          <Heading as="h2" className={styles.h2}>
            The services you reach for. One line each.
          </Heading>
          <p className={styles.lead}>
            Each runs in the cluster with its usual port on 127.0.0.1, a .localhost name, and a throwaway password
            for development.
          </p>
        </Reveal>
        <ul className={styles.chips}>
          {APPS.map((a) => (
            <li key={a}>{a}</li>
          ))}
        </ul>
      </div>
    </section>
  );
}

function Honest() {
  return (
    <section className={`${styles.section} ${styles.alt}`}>
      <div className="container">
        <Reveal>
          <div className={styles.honest}>
            <Heading as="h2" className={styles.h3}>
              What it is not
            </Heading>
            <ul>
              <li>One server, no high availability. Extra nodes are agents only.</li>
              <li>The containers share the VM's kernel, unlike the per-container microVMs of the sandboxes.</li>
              <li>Checkpoints need a single-node cluster and restore on the same kind of Mac.</li>
              <li>The first boot needs internet unless you use --offline.</li>
            </ul>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function Cta(): ReactNode {
  return (
    <section className={styles.cta}>
      <div className="container">
        <Heading as="h2" className={styles.ctaTitle}>
          Start a cluster in one command.
        </Heading>
        <div className={styles.buttons}>
          <Link className={styles.primary} to="/docs/kubernetes-on-mac">
            Kubernetes on a Mac
          </Link>
          <Link className={styles.secondary} href={REPO}>
            GitHub
          </Link>
        </div>
        <p className={styles.credit}>
          Built with <a href="https://claude.com/claude-code">Claude Code</a> and tested on a real Mac mini.
        </p>
      </div>
    </section>
  );
}

export default function Kubernetes(): ReactNode {
  return (
    <Layout
      title="Kubernetes on a Mac"
      description="A real k3s cluster on Apple silicon that behaves like Linux, with checkpoints of the whole running machine.">
      <Hero />
      <main>
        <Stats />
        <Features />
        <Apps />
        <Honest />
        <Cta />
      </main>
    </Layout>
  );
}
