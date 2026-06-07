import { describe, it, expect, vi } from 'vitest';

// projects.ts imports `app` from electron (for the projects.json path); stub it so
// the pure repoNameFromUrl helper can be imported and tested under plain Node.
vi.mock('electron', () => ({ app: { getPath: () => '/tmp' } }));

import { repoNameFromUrl } from './projects';

describe('repoNameFromUrl', () => {
  it('parses an https GitHub URL with .git', () => {
    expect(repoNameFromUrl('https://github.com/Eyalm321/hyperpanes.git')).toBe('hyperpanes');
  });
  it('parses an https URL without .git', () => {
    expect(repoNameFromUrl('https://github.com/owner/my-repo')).toBe('my-repo');
  });
  it('parses an scp-style SSH URL', () => {
    expect(repoNameFromUrl('git@github.com:owner/my-repo.git')).toBe('my-repo');
  });
  it('parses an ssh:// URL and keeps dots in the name', () => {
    expect(repoNameFromUrl('ssh://git@github.com/owner/My.Repo.git')).toBe('My.Repo');
  });
  it('strips a trailing slash', () => {
    expect(repoNameFromUrl('https://gitlab.com/group/sub/proj/')).toBe('proj');
  });
  it('returns null for an empty string', () => {
    expect(repoNameFromUrl('')).toBeNull();
  });
});
