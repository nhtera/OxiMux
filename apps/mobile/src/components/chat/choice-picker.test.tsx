import { fireEvent, render, screen } from '@testing-library/react-native';
import type { Choice } from 'oximux-core';

import { ChoicePicker } from '@/components/chat/choice-picker';

const CHOICES: Choice[] = [
  { id: 'opus-4.8', label: 'Opus 4.8', description: 'most capable' },
  { id: 'sonnet-5', label: 'Sonnet 5', description: undefined },
];

describe('ChoicePicker', () => {
  it('shows each choice by its human label, not its wire id', () => {
    render(
      <ChoicePicker
        title="Model"
        choices={CHOICES}
        current="opus-4.8"
        visible
        busy={false}
        onPick={() => {}}
        onClose={() => {}}
      />
    );
    // The id is what crosses the wire; the label is what a person reads. A
    // picker listing `claude-opus-4-8` would be technically correct and unusable.
    expect(screen.getByText('Opus 4.8')).toBeTruthy();
    expect(screen.getByText('most capable')).toBeTruthy();
    expect(screen.queryByText('opus-4.8')).toBeNull();
  });

  it('reports the picked id', () => {
    const onPick = jest.fn();
    render(
      <ChoicePicker
        title="Model"
        choices={CHOICES}
        current="opus-4.8"
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
        current="opus-4.8"
        visible
        busy={false}
        onPick={onPick}
        onClose={() => {}}
      />
    );
    fireEvent.press(screen.getByText('Opus 4.8'));
    expect(onPick).not.toHaveBeenCalled();
  });

  it('refuses input while a switch is in flight', () => {
    const onPick = jest.fn();
    render(
      <ChoicePicker
        title="Model"
        choices={CHOICES}
        current="opus-4.8"
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
