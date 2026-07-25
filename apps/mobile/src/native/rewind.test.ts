import { rewindTargets, targetLabel } from '@/native/rewind';
import type { ThreadEntry } from '@/native/thread';

function user(text: string, checkpoint: { sha: string; show: boolean } | null = null): ThreadEntry {
  return { User: { text, images: [], checkpoint } };
}

function assistant(text: string): ThreadEntry {
  return { Assistant: { text, thinking: '' } };
}

describe('rewindTargets', () => {
  it('offers every user turn, including the last', () => {
    const targets = rewindTargets([user('one'), assistant('a'), user('two'), assistant('b')]);

    expect(targets.map((t) => t.text)).toEqual(['one', 'two']);
    // Rewinding to the last turn drops that prompt and its reply — "undo my
    // last message" is a real thing to want.
    expect(targets[1].ordinal).toBe(1);
  });

  it('counts every entry a rewind removes, not just user turns', () => {
    const targets = rewindTargets([
      user('one'),
      assistant('a'),
      user('two'),
      assistant('b'),
      assistant('c'),
    ]);

    // Rewinding to 'two' drops it plus both assistant entries after it.
    // Reporting "1 message" here would understate a destructive action.
    expect(targets[1].messagesRemoved).toBe(3);
    expect(targets[0].messagesRemoved).toBe(5);
  });

  it('ordinals count only user entries so they match the wire', () => {
    const targets = rewindTargets([
      assistant('preamble'),
      user('one'),
      assistant('a'),
      assistant('b'),
      user('two'),
    ]);

    // Ordinal is position among users; entryIndex is position in the list.
    // Conflating them would truncate at the wrong point.
    expect(targets.map((t) => t.ordinal)).toEqual([0, 1]);
    expect(targets.map((t) => t.entryIndex)).toEqual([1, 4]);
  });

  it('offers the files axis only when the turn actually changed the repo', () => {
    const targets = rewindTargets([
      user('untouched', { sha: 'abc', show: false }),
      user('changed files', { sha: 'def', show: true }),
      user('no checkpoint', null),
    ]);

    // A checkpoint that exists but is not flagged `show` means the turn changed
    // nothing — offering to restore files would be a no-op dressed as a choice.
    expect(targets.map((t) => t.filesAvailable)).toEqual([false, true, false]);
  });

  it('returns nothing for a transcript with no user turns', () => {
    expect(rewindTargets([assistant('a')])).toEqual([]);
    expect(rewindTargets([])).toEqual([]);
  });
});

describe('targetLabel', () => {
  it('collapses newlines so a multi-line prompt stays one row', () => {
    const [target] = rewindTargets([user('first line\n\nsecond line')]);
    expect(targetLabel(target)).toBe('first line second line');
  });

  it('clips a long prompt to the requested width', () => {
    const [target] = rewindTargets([user('x'.repeat(100))]);
    const label = targetLabel(target, 20);
    expect(label).toHaveLength(20);
    expect(label.endsWith('…')).toBe(true);
  });

  it('names an empty message rather than rendering a blank row', () => {
    const [target] = rewindTargets([user('   ')]);
    expect(targetLabel(target)).toBe('(empty message)');
  });
});
