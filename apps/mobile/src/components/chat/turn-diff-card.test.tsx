import { fireEvent, render, screen } from '@testing-library/react-native';

import { TurnDiffCard } from '@/components/chat/turn-diff-card';
import type { TurnFileChange } from '@/native/thread';
import * as unifiedDiff from '@/native/unified-diff';

// Spied rather than asserted through the rendered output. The first version of
// the lazy-parse test below checked that the diff text was absent while
// collapsed — and passed against a deliberately eager implementation, because
// the expanded body is not mounted when collapsed, so the text is missing either
// way. That test measured rendering and claimed to measure parsing.
const parseSpy = jest.spyOn(unifiedDiff, 'parseUnifiedDiff');

beforeEach(() => parseSpy.mockClear());

const FILES: TurnFileChange[] = [
  { path: 'src/main.rs', added: 40, removed: 2 },
  { path: 'src/lib.rs', added: 2, removed: 6 },
];

const DIFF = [
  'diff --git a/src/main.rs b/src/main.rs',
  '--- a/src/main.rs',
  '+++ b/src/main.rs',
  '@@ -1,2 +1,2 @@',
  '-let old = 1;',
  '+let fresh = 2;',
].join('\n');

describe('TurnDiffCard', () => {
  it('summarises the turn without being expanded', () => {
    render(<TurnDiffCard files={FILES} diff={null} />);
    expect(screen.getByText(/2 files changed/)).toBeTruthy();
    expect(screen.getByText('+42')).toBeTruthy();
    expect(screen.getByText('−8')).toBeTruthy();
  });

  it('does not parse the diff until expanded', () => {
    render(<TurnDiffCard files={FILES} diff={DIFF} />);
    // The guarantee is that the *work* is not done, not merely that the result
    // is not shown: a transcript can hold several of these, each carrying a
    // thousand-line diff, and scrolling must not pay to parse them all.
    expect(parseSpy).not.toHaveBeenCalled();

    fireEvent.press(screen.getByText(/2 files changed/));
    expect(parseSpy).toHaveBeenCalledWith(DIFF);
  });

  it('shows the diff hunks once expanded', () => {
    render(<TurnDiffCard files={FILES} diff={DIFF} />);
    fireEvent.press(screen.getByText(/2 files changed/));

    // Rendered through the git screen's DiffView, which re-adds the +/- marker.
    expect(screen.getByText(/let fresh = 2;/)).toBeTruthy();
    expect(screen.getByText(/let old = 1;/)).toBeTruthy();
  });

  it('explains itself when the backend reported no diff', () => {
    render(<TurnDiffCard files={FILES} diff={null} />);
    fireEvent.press(screen.getByText(/2 files changed/));

    // A dead tap would read as a bug. Claude/ACP/Pi genuinely do not report a
    // per-turn diff, so the card says so and points somewhere useful.
    expect(screen.getByText(/doesn't report line-by-line changes/)).toBeTruthy();
    // The per-file stats are still worth showing in that case.
    expect(screen.getByText(/src\/main\.rs/)).toBeTruthy();
  });

  it('reports a diff that was present but unreadable, distinctly from none at all', () => {
    render(<TurnDiffCard files={FILES} diff={'this is not a diff'} />);
    fireEvent.press(screen.getByText(/2 files changed/));
    // Distinguished on purpose: "the agent sent nothing" and "the agent sent
    // something we failed to read" are different problems, and collapsing them
    // would hide a parser bug behind an expected-looking message.
    expect(screen.getByText(/could not be read/)).toBeTruthy();
  });
});
