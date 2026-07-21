import {
  attachPromptText,
  checkStatus,
  checksSettled,
  checksSummary,
  relativeAge,
} from '@/native/forge';
import type { CheckRun, ForgeItem } from 'oximux-core';

function check(bucket: string): CheckRun {
  return { name: 'build', bucket, link: '', description: '' };
}

function item(overrides: Partial<ForgeItem> = {}): ForgeItem {
  return {
    number: 42n,
    title: 'Fix the parser',
    state: 'OPEN',
    url: 'https://example.test/pull/42',
    labels: [],
    assignees: [],
    author: 'someone',
    updatedAt: '',
    ...overrides,
  };
}

describe('checkStatus', () => {
  it('maps the forge buckets it knows', () => {
    expect(checkStatus(check('pass'))).toBe('pass');
    expect(checkStatus(check('fail'))).toBe('fail');
    expect(checkStatus(check('pending'))).toBe('pending');
    expect(checkStatus(check('skipping'))).toBe('skipped');
    expect(checkStatus(check('cancel'))).toBe('skipped');
  });

  it('is case-insensitive', () => {
    expect(checkStatus(check('PASS'))).toBe('pass');
    expect(checkStatus(check('Failure'))).toBe('fail');
  });

  it('calls an unfamiliar bucket unknown rather than guessing failure', () => {
    // Guessing "failed" for something merely unfamiliar would be alarming and
    // wrong; a neutral state is the honest reading.
    expect(checkStatus(check('neutral'))).toBe('unknown');
    expect(checkStatus(check(''))).toBe('unknown');
  });
});

describe('checksSettled', () => {
  it('is true only when nothing is pending', () => {
    expect(checksSettled([check('pass'), check('fail')])).toBe(true);
    expect(checksSettled([check('pass'), check('pending')])).toBe(false);
  });

  it('treats no checks as settled', () => {
    // Vacuously true, and the right answer: there is nothing to wait for.
    expect(checksSettled([])).toBe(true);
  });
});

describe('checksSummary', () => {
  it('counts each state', () => {
    const summary = checksSummary([check('pass'), check('pass'), check('fail')]);
    expect(summary).toBe('2 passed, 1 failed');
  });

  it('omits states with no members', () => {
    expect(checksSummary([check('pass')])).toBe('1 passed');
  });

  it('is empty for no checks rather than claiming zero passed', () => {
    // "0 passed" would imply checks ran and none succeeded.
    expect(checksSummary([])).toBe('');
  });
});

describe('relativeAge', () => {
  const now = Date.parse('2026-07-21T12:00:00Z');

  it('renders coarser units as the age grows', () => {
    expect(relativeAge('2026-07-21T11:59:30Z', now)).toBe('now');
    expect(relativeAge('2026-07-21T11:30:00Z', now)).toBe('30m');
    expect(relativeAge('2026-07-21T09:00:00Z', now)).toBe('3h');
    expect(relativeAge('2026-07-18T12:00:00Z', now)).toBe('3d');
    expect(relativeAge('2024-07-21T12:00:00Z', now)).toBe('2y');
  });

  it('reads a future timestamp as now, not a negative age', () => {
    // Clock skew between desktop, forge and phone makes this reachable with
    // nothing actually wrong — and "in 3 hours" on a list of past events reads
    // as a bug.
    expect(relativeAge('2026-07-21T15:00:00Z', now)).toBe('now');
  });

  it('is empty when the timestamp is missing or unparseable', () => {
    expect(relativeAge('', now)).toBe('');
    expect(relativeAge('not a date', now)).toBe('');
  });
});

describe('attachPromptText', () => {
  it('leads with the item and its link', () => {
    const text = attachPromptText(item(), 'The parser drops trailing commas.');
    expect(text).toBe(
      'Fix the parser (#42)\nhttps://example.test/pull/42\n\nThe parser drops trailing commas.'
    );
  });

  it('omits the body section when there is none', () => {
    // `null` (the forge could not supply it) and an empty body both mean there
    // is nothing more to say — neither should leave a dangling blank line.
    expect(attachPromptText(item(), null)).toBe(
      'Fix the parser (#42)\nhttps://example.test/pull/42'
    );
    expect(attachPromptText(item(), '   ')).toBe(
      'Fix the parser (#42)\nhttps://example.test/pull/42'
    );
  });
});
