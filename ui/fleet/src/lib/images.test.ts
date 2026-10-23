import { describe, expect, it } from 'vitest';
import { draftOf, emptyImageDraft, imageProblem, imageRequest } from './images';

const ok = { ...emptyImageDraft(), name: 'macos-27-xcode', image: '/Users/fluxvm/vms/x/disk.raw' };

describe('image catalog form', () => {
  it('accepts a complete draft', () => expect(imageProblem(ok, false, [])).toBe(''));
  it('refuses what central would', () => {
    expect(imageProblem({ ...ok, name: 'Bad Name' }, false, [])).toMatch(/lowercase/);
    expect(imageProblem(ok, false, ['macos-27-xcode'])).toMatch(/already/);
    expect(imageProblem(ok, true, ['macos-27-xcode'])).toBe('');
    expect(imageProblem({ ...ok, image: '' }, false, [])).toMatch(/disk path/);
    expect(imageProblem({ ...ok, image: 'disk.raw' }, false, [])).toMatch(/absolute/);
    expect(imageProblem({ ...ok, vcpus: '0' }, false, [])).toMatch(/vCPUs/);
    expect(imageProblem({ ...ok, vcpus: '65' }, false, [])).toMatch(/vCPUs/);
    expect(imageProblem({ ...ok, memoryGib: '0.5' }, false, [])).toMatch(/Memory/);
    expect(imageProblem({ ...ok, sshUser: 'bad user' }, false, [])).toMatch(/SSH user/);
  });
  it('sends memory in MiB and leaves empty optionals out', () => {
    expect(imageRequest({ ...ok, memoryGib: '8', displayName: ' ' })).toEqual({
      guest_os: 'macos', image: '/Users/fluxvm/vms/x/disk.raw', vcpus: 4, memory_mib: 8192,
      display_name: undefined, description: undefined, source: undefined, disk_size_gib: undefined, ssh_user: undefined,
    });
  });
  it('keeps the disk size of an edited image and round-trips a draft', () => {
    const img = { name: 'a', guest_os: 'linux' as const, image: '/d', vcpus: 2, memory_mib: 2560, disk_size_gib: 40, display_name: 'A' };
    expect(draftOf(img).memoryGib).toBe('2.5');
    expect(imageRequest(draftOf(img), img).disk_size_gib).toBe(40);
    expect(imageRequest(draftOf(img), img).memory_mib).toBe(2560);
  });
});
