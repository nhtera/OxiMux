import type { ProjectSummary, SessionSummary } from 'oximux-core';

import { basename, groupSessionsByProject } from './session-grouping';

function session(sessionId: string, title: string): SessionSummary {
  return { sessionId, title, model: undefined, lastSeq: 0n, awaitingPermission: false };
}

function project(name: string, path: string): ProjectSummary {
  return { name, path };
}

describe('basename', () => {
  it('returns the final path component', () => {
    expect(basename('/Users/me/Code/OxiMux')).toBe('OxiMux');
  });

  it('ignores a trailing slash', () => {
    expect(basename('/Users/me/work/')).toBe('work');
  });
});

describe('groupSessionsByProject', () => {
  it('files each session under the project its folded title names', () => {
    const projects = [project('OxiMux', '/Users/me/Code/OxiMux'), project('work', '/Users/me/work')];
    const sessions = [session('a', 'OxiMux · Chat 1'), session('b', 'work · fix bug')];
    const groups = groupSessionsByProject(projects, sessions);
    expect(groups.map((g) => g.name)).toEqual(['OxiMux', 'work']);
    expect(groups[0].data.map((s) => s.sessionId)).toEqual(['a']);
    expect(groups[1].data.map((s) => s.sessionId)).toEqual(['b']);
    // Each project carries its path so the "+" can start a session there.
    expect(groups[0].path).toBe('/Users/me/Code/OxiMux');
  });

  it('keeps a project with no sessions so a session can still be started in it', () => {
    const projects = [project('OxiMux', '/Users/me/Code/OxiMux'), project('empty', '/Users/me/empty')];
    const groups = groupSessionsByProject(projects, [session('a', 'OxiMux · Chat 1')]);
    expect(groups.map((g) => g.name)).toEqual(['OxiMux', 'empty']);
    expect(groups[1].data).toEqual([]);
  });

  it('collects sessions matching no project into a trailing Other bucket', () => {
    const projects = [project('OxiMux', '/Users/me/Code/OxiMux')];
    const groups = groupSessionsByProject(projects, [
      session('a', 'OxiMux · Chat 1'),
      session('b', 'ghost · orphan'),
    ]);
    expect(groups.map((g) => g.name)).toEqual(['OxiMux', 'Other']);
    expect(groups[1].path).toBeUndefined();
    expect(groups[1].data.map((s) => s.sessionId)).toEqual(['b']);
  });

  it('omits the Other bucket when every session matched', () => {
    const projects = [project('OxiMux', '/Users/me/Code/OxiMux')];
    const groups = groupSessionsByProject(projects, [session('a', 'OxiMux · Chat 1')]);
    expect(groups.map((g) => g.name)).toEqual(['OxiMux']);
  });

  it('degrades to one flat pathless group when the host lists no projects', () => {
    const groups = groupSessionsByProject([], [session('a', 'OxiMux · Chat 1'), session('b', 'hi')]);
    expect(groups).toHaveLength(1);
    expect(groups[0].path).toBeUndefined();
    expect(groups[0].data.map((s) => s.sessionId)).toEqual(['a', 'b']);
  });
});
