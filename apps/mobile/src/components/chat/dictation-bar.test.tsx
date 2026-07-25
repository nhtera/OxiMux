import { fireEvent, render, screen } from '@testing-library/react-native';

import { DictationBar, SAMPLE_TEST_ID } from '@/components/chat/dictation-bar';
import { WAVEFORM_SAMPLES } from '@/native/use-dictation';

/**
 * The bar that replaces the composer while the microphone is open.
 *
 * Worth testing rather than eyeballing because two of its properties are easy to
 * regress and invisible in a screenshot of a quiet room: that the trace actually
 * tracks the levels it is handed, and that ending a dictation cannot be triggered
 * twice while the first ending is still in flight.
 */
function setup(props: Partial<React.ComponentProps<typeof DictationBar>> = {}) {
  const handlers = { onCancel: jest.fn(), onStop: jest.fn(), onSend: jest.fn() };
  render(<DictationBar phase="recording" levels={[]} {...handlers} {...props} />);
  return handlers;
}

/** Heights of the trace's bars, left (oldest) to right (newest). */
function barHeights(): number[] {
  return screen.queryAllByTestId(SAMPLE_TEST_ID).map((bar) => {
    const styles = [bar.props.style].flat(2) as ({ height?: number } | undefined)[];
    return styles.find((s) => s?.height !== undefined)?.height ?? 0;
  });
}

it('draws one bar per sample slot even before the trace has filled', () => {
  setup({ levels: [0.5] });

  // A trace that grew from the right edge for its first four seconds would read
  // as a progress bar. It is padded so it starts full-width and travels.
  expect(barHeights()).toHaveLength(WAVEFORM_SAMPLES);
});

it('scales each bar to its sample, loudest last', () => {
  setup({ levels: [0, 0.5, 1] });

  const heights = barHeights();
  const [quiet, mid, loud] = heights.slice(-3);
  expect(quiet).toBeLessThan(mid);
  expect(mid).toBeLessThan(loud);
  // Silence still draws something — a resting dot, not a gap in the row.
  expect(quiet).toBeGreaterThan(0);
});

it('clamps a sample that arrives outside 0..1', () => {
  setup({ levels: [1] });
  const ceiling = barHeights().at(-1);

  screen.unmount();
  setup({ levels: [4] });

  // Metering is a mapped dBFS reading, so an out-of-range value is a plausible
  // input; it must not push a bar out through the top of the dock.
  expect(barHeights().at(-1)).toBe(ceiling);
});

it('offers the three ways out of a recording', () => {
  const { onCancel, onStop, onSend } = setup();

  fireEvent.press(screen.getByLabelText('Discard this dictation'));
  fireEvent.press(screen.getByLabelText('Stop dictating and keep the text'));
  fireEvent.press(screen.getByLabelText('Stop dictating and send'));

  expect(onCancel).toHaveBeenCalledTimes(1);
  expect(onStop).toHaveBeenCalledTimes(1);
  expect(onSend).toHaveBeenCalledTimes(1);
});

it('shows the transcript is being fetched, with no levels left to draw', () => {
  setup({ phase: 'transcribing', levels: [0.4, 0.9] });

  expect(screen.getAllByText('Transcribing').length).toBeGreaterThan(0);
  expect(barHeights()).toHaveLength(0);
});

it('cannot be ended a second time while the transcript is in flight', () => {
  const { onStop, onSend, onCancel } = setup({ phase: 'transcribing' });

  fireEvent.press(screen.getByLabelText('Stop dictating and keep the text'));
  fireEvent.press(screen.getByLabelText('Stop dictating and send'));
  expect(onStop).not.toHaveBeenCalled();
  expect(onSend).not.toHaveBeenCalled();

  // Discarding stays live, though — it is the only thing left that a user who
  // started dictating by accident can still do.
  fireEvent.press(screen.getByLabelText('Discard this dictation'));
  expect(onCancel).toHaveBeenCalledTimes(1);
});
