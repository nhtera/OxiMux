import { fireEvent, render, screen } from '@testing-library/react-native';

import { KeyBar } from './key-bar';

function setup(overrides: Partial<React.ComponentProps<typeof KeyBar>> = {}) {
  const props = {
    onKey: jest.fn(),
    onCtrlToggle: jest.fn(),
    onZoom: jest.fn(),
    onScroll: jest.fn(),
    ctrlArmed: false,
    ...overrides,
  };
  render(<KeyBar {...props} />);
  return props;
}

describe('KeyBar', () => {
  // The sequences are the contract with the host: it receives raw bytes and has
  // no idea a phone sent them, so a wrong escape is indistinguishable from a
  // broken keyboard.
  it.each([
    ['esc', '\x1b'],
    ['tab', '\t'],
    ['^C', '\x03'],
    ['^D', '\x04'],
    ['up', '\x1b[A'],
    ['down', '\x1b[B'],
    ['left', '\x1b[D'],
    ['right', '\x1b[C'],
  ])('sends %s as the bytes a real terminal would emit', (label, sequence) => {
    const { onKey } = setup();
    fireEvent.press(screen.getByLabelText(label));
    expect(onKey).toHaveBeenCalledWith(sequence);
  });

  it('routes Ctrl through the toggle, not the byte stream', () => {
    // Ctrl is a modifier applied in the page to the *next* keystroke; sending a
    // byte here would type garbage instead of arming anything.
    const { onKey, onCtrlToggle } = setup();
    fireEvent.press(screen.getByLabelText('ctrl'));
    expect(onCtrlToggle).toHaveBeenCalled();
    expect(onKey).not.toHaveBeenCalled();
  });

  it('marks Ctrl selected while it is armed', () => {
    setup({ ctrlArmed: true });
    expect(screen.getByLabelText('ctrl').props.accessibilityState).toMatchObject({ selected: true });
  });

  it('scrolls by whole lines and zooms by single points', () => {
    const { onScroll, onZoom } = setup();
    fireEvent.press(screen.getByLabelText('page up'));
    expect(onScroll).toHaveBeenCalledWith(-10);
    fireEvent.press(screen.getByLabelText('page down'));
    expect(onScroll).toHaveBeenCalledWith(10);
    fireEvent.press(screen.getByLabelText('larger text'));
    expect(onZoom).toHaveBeenCalledWith(1);
    fireEvent.press(screen.getByLabelText('smaller text'));
    expect(onZoom).toHaveBeenCalledWith(-1);
  });
});
