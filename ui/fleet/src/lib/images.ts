// The image catalog: what a tenant can pick when launching. The disk must already be on every Mac that should run it.

export interface CatalogImage {
  name: string;
  display_name?: string;
  description?: string;
  guest_os: 'macos' | 'linux';
  image: string;
  source?: string;
  vcpus: number;
  memory_mib: number;
  disk_size_gib?: number;
  ssh_user?: string;
}

export interface ImageDraft {
  name: string;
  displayName: string;
  guest: 'macos' | 'linux';
  source: string;
  image: string;
  vcpus: string;
  memoryGib: string;
  description: string;
  sshUser: string;
}

export const emptyImageDraft = (): ImageDraft => ({ name: '', displayName: '', guest: 'macos', source: '', image: '', vcpus: '4', memoryGib: '8', description: '', sshUser: '' });

export const draftOf = (i: CatalogImage): ImageDraft => ({
  name: i.name, displayName: i.display_name ?? '', guest: i.guest_os, source: i.source ?? '', image: i.image, vcpus: String(i.vcpus),
  memoryGib: String(+(i.memory_mib / 1024).toFixed(2)), description: i.description ?? '', sshUser: i.ssh_user ?? '',
});

export function imageProblem(d: ImageDraft, editing: boolean, existing: string[]): string {
  if (!/^[a-z0-9][a-z0-9._-]{0,62}$/.test(d.name)) return 'The ID is lowercase letters, digits, dot, dash and underscore, starting with a letter or digit, up to 63 characters.';
  if (!editing && existing.includes(d.name)) return 'An image with that ID is already in the catalog.';
  if (!d.image.trim()) return 'Give the disk path on the Macs.';
  if (!d.image.startsWith('/')) return 'The disk path is absolute, starting with /.';
  const cpus = Number(d.vcpus);
  if (!Number.isInteger(cpus) || cpus < 1 || cpus > 64) return 'vCPUs is a whole number from 1 to 64.';
  const mem = Number(d.memoryGib);
  if (!Number.isFinite(mem) || mem < 1) return 'Memory is at least 1 GiB.';
  if (d.sshUser && !/^[A-Za-z0-9._][A-Za-z0-9._-]{0,31}$/.test(d.sshUser)) return 'The SSH user is up to 32 letters, digits, dot, dash or underscore.';
  return '';
}

/** The PUT body: empty optional fields are left out, and the disk size of an edited image is kept. */
export function imageRequest(d: ImageDraft, previous?: CatalogImage): Omit<CatalogImage, 'name'> {
  const opt = (v: string) => v.trim() || undefined;
  return {
    guest_os: d.guest, image: d.image.trim(), vcpus: Number(d.vcpus), memory_mib: Math.round(Number(d.memoryGib) * 1024),
    display_name: opt(d.displayName), description: opt(d.description), source: opt(d.source), disk_size_gib: previous?.disk_size_gib, ssh_user: opt(d.sshUser),
  };
}
