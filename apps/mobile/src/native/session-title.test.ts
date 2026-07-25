import { parseSessionTitle } from './session-title';

describe('parseSessionTitle', () => {
  it('splits a project-folded title into project + label', () => {
    expect(parseSessionTitle('OxiMux · Fix parser')).toEqual({
      project: 'OxiMux',
      label: 'Fix parser',
    });
  });

  it('leaves an un-folded title as the label with no project (older host)', () => {
    expect(parseSessionTitle('agent-2')).toEqual({ label: 'agent-2' });
  });

  it('keeps a later separator inside the label (splits on the first only)', () => {
    expect(parseSessionTitle('OxiMux · a · b')).toEqual({
      project: 'OxiMux',
      label: 'a · b',
    });
  });

  it('treats a leading separator as un-folded (no empty project)', () => {
    expect(parseSessionTitle(' · orphan')).toEqual({ label: ' · orphan' });
  });
});
