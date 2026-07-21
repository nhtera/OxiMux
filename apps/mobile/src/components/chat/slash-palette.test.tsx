import { fireEvent, render, screen } from '@testing-library/react-native';

import { filterCommands, SlashPalette, slashQuery } from '@/components/chat/slash-palette';

describe('slashQuery', () => {
  it('opens on a leading slash and returns the typed name', () => {
    expect(slashQuery('/')).toBe('');
    expect(slashQuery('/cle')).toBe('cle');
  });

  it('stays closed for a slash that is not at the start', () => {
    // A slash mid-sentence is a path, a fraction, or a date far more often than
    // it is a command — opening there would interrupt ordinary typing.
    expect(slashQuery('see src/main.rs')).toBeUndefined();
    expect(slashQuery('and/or')).toBeUndefined();
    expect(slashQuery('on 21/07')).toBeUndefined();
  });

  it('closes once the command name is followed by a space', () => {
    // By then the user is writing arguments, and the palette would sit over the
    // input for no reason.
    expect(slashQuery('/clear ')).toBeUndefined();
    expect(slashQuery('/model opus')).toBeUndefined();
  });

  it('stays closed for ordinary text', () => {
    expect(slashQuery('hello')).toBeUndefined();
    expect(slashQuery('')).toBeUndefined();
  });
});

describe('filterCommands', () => {
  const COMMANDS = ['clear', 'compact', 'model'];

  it('returns everything for a bare slash', () => {
    expect(filterCommands(COMMANDS, '')).toEqual(COMMANDS);
  });

  it('narrows by substring, case-insensitively', () => {
    expect(filterCommands(COMMANDS, 'c')).toEqual(['clear', 'compact']);
    expect(filterCommands(COMMANDS, 'MOD')).toEqual(['model']);
  });

  it('returns nothing when no command matches', () => {
    expect(filterCommands(COMMANDS, 'zzz')).toEqual([]);
  });
});

describe('SlashPalette', () => {
  it('lists commands with their slash prefix', () => {
    render(<SlashPalette commands={['clear', 'model']} onSelect={() => {}} />);
    expect(screen.getByText('/clear')).toBeTruthy();
    expect(screen.getByText('/model')).toBeTruthy();
  });

  it('renders nothing when the agent exposes no commands', () => {
    // An older host, or an agent with none configured. An empty dropdown would
    // read as broken.
    const { toJSON } = render(<SlashPalette commands={[]} onSelect={() => {}} />);
    expect(toJSON()).toBeNull();
  });

  it('reports the chosen command', () => {
    const onSelect = jest.fn();
    render(<SlashPalette commands={['compact']} onSelect={onSelect} />);
    fireEvent.press(screen.getByText('/compact'));
    expect(onSelect).toHaveBeenCalledWith('compact');
  });
});
