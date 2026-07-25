import { fireEvent, render, screen } from '@testing-library/react-native';
import type { Choice } from 'oximux-core';

import { ChoicePicker } from '@/components/chat/choice-picker';

const CHOICES: Choice[] = [
  { id: 'claude-opus-5', label: 'Opus 5', description: 'most capable' },
  { id: 'sonnet-5', label: 'Sonnet 5', description: undefined },
];

describe('ChoicePicker', () => {
  it('shows each choice by its human label, not its wire id', () => {
    render(
      <ChoicePicker
        title="Model"
        choices={CHOICES}
        current="claude-opus-5"
        visible
        busy={false}
        onPick={() => {}}
        onClose={() => {}}
      />
    );
    // The id is what crosses the wire; the label is what a person reads. A
    // picker listing `claude-opus-5` would be technically correct and unusable.
    expect(screen.getByText('Opus 5')).toBeTruthy();
    expect(screen.getByText('most capable')).toBeTruthy();
    expect(screen.queryByText('claude-opus-5')).toBeNull();
  });

  it('reports the picked id', () => {
    const onPick = jest.fn();
    render(
      <ChoicePicker
        title="Model"
        choices={CHOICES}
        current="claude-opus-5"
        visible
        busy={false}
        onPick={onPick}
        onClose={() => {}}
      />
    );
    fireEvent.press(screen.getByText('Sonnet 5'));
    expect(onPick).toHaveBeenCalledWith('sonnet-5');
  });

  it('does not re-pick the model already running', () => {
    // Switching costs a round trip and can be refused outright, so re-selecting
    // what is already active is a pointless call, not a harmless one.
    const onPick = jest.fn();
    render(
      <ChoicePicker
        title="Model"
        choices={CHOICES}
        current="claude-opus-5"
        visible
        busy={false}
        onPick={onPick}
        onClose={() => {}}
      />
    );
    fireEvent.press(screen.getByText('Opus 5'));
    expect(onPick).not.toHaveBeenCalled();
  });

  it('refuses input while a switch is in flight', () => {
    const onPick = jest.fn();
    render(
      <ChoicePicker
        title="Model"
        choices={CHOICES}
        current="claude-opus-5"
        visible
        busy
        onPick={onPick}
        onClose={() => {}}
      />
    );
    fireEvent.press(screen.getByText('Sonnet 5'));
    expect(onPick).not.toHaveBeenCalled();
  });
});
