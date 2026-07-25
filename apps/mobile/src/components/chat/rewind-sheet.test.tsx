import { fireEvent, render, screen } from '@testing-library/react-native';

import { RewindSheet } from '@/components/chat/rewind-sheet';
import type { ThreadEntry } from '@/native/thread';

function user(text: string): ThreadEntry {
  return { User: { text, images: [], checkpoint: null } };
}
function assistant(text: string): ThreadEntry {
  return { Assistant: { text, thinking: '' } };
}

const ENTRIES = [user('first prompt'), assistant('a'), user('second prompt'), assistant('b')];

function setup(overrides: Partial<Parameters<typeof RewindSheet>[0]> = {}) {
  const onRewind = jest.fn();
  const onClose = jest.fn();
  render(
    <RewindSheet
      visible
      entries={ENTRIES}
      busy={false}
      onRewind={onRewind}
      onClose={onClose}
      {...overrides}
    />
  );
  return { onRewind, onClose };
}

it('lists the user turns newest first', () => {
  setup();
  // Newest first: the turn someone wants to undo is almost always recent.
  const rows = screen.getAllByText(/prompt$/);
  expect(rows[0]).toHaveTextContent('second prompt');
  expect(rows[1]).toHaveTextContent('first prompt');
});

it('does not rewind on the first tap', () => {
  const { onRewind } = setup();

  fireEvent.press(screen.getByText('second prompt'));

  // Tapping a row opens the confirm step. A rewind destroys conversation, so it
  // must never be one mis-tap away.
  expect(onRewind).not.toHaveBeenCalled();
  expect(screen.getByText(/Remove 2 messages/)).toBeTruthy();
});

it('sends the ordinal, not the entry index', () => {
  const { onRewind } = setup();

  fireEvent.press(screen.getByText('second prompt'));
  fireEvent.press(screen.getByText('Rewind'));

  // 'second prompt' is entry index 2 but user ordinal 1. Sending the index
  // would truncate at the wrong point — silently, and destructively.
  expect(onRewind).toHaveBeenCalledWith(1);
});

it('states the blast radius before confirming', () => {
  setup();

  fireEvent.press(screen.getByText('first prompt'));

  // Rewinding to the first prompt removes all four entries, not "1 message".
  expect(screen.getByText(/Remove 4 messages/)).toBeTruthy();
});

it('says files are untouched, since the phone cannot restore them', () => {
  setup();

  fireEvent.press(screen.getByText('second prompt'));

  // A user who assumed a rewind also reverts their code would otherwise find
  // out by looking at a repo that no longer matches the conversation.
  expect(screen.getByText(/Files on disk are left as they are/)).toBeTruthy();
});

it('can back out of the confirm step without rewinding', () => {
  const { onRewind } = setup();

  fireEvent.press(screen.getByText('second prompt'));
  fireEvent.press(screen.getByText('Back'));

  expect(onRewind).not.toHaveBeenCalled();
  expect(screen.getByText('second prompt')).toBeTruthy();
});

it('surfaces a failure instead of appearing to have worked', () => {
  setup({ error: 'that message is no longer in the transcript' });

  fireEvent.press(screen.getByText('second prompt'));

  expect(screen.getByText(/no longer in the transcript/)).toBeTruthy();
});

it('blocks a second submit while one is in flight', () => {
  const { onRewind } = setup({ busy: true });

  fireEvent.press(screen.getByText('second prompt'));
  fireEvent.press(screen.getByText('Rewind'));

  // Two rewinds racing would have the second fork a session the first is still
  // replacing; the host refuses it, but the UI should not send it either.
  expect(onRewind).not.toHaveBeenCalled();
});

it('says so when there is nothing to rewind to', () => {
  render(
    <RewindSheet
      visible
      entries={[assistant('only a reply')]}
      busy={false}
      onRewind={jest.fn()}
      onClose={jest.fn()}
    />
  );

  expect(screen.getByText(/Nothing to rewind to/)).toBeTruthy();
});
