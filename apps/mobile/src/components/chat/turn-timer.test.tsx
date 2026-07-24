import { formatDuration } from '@/components/chat/turn-timer';

describe('formatDuration', () => {
  it('shows whole seconds under a minute', () => {
    expect(formatDuration(0)).toBe('0s');
    expect(formatDuration(12_000)).toBe('12s');
    expect(formatDuration(59_000)).toBe('59s');
  });

  it('rolls into minutes with zero-padded seconds', () => {
    expect(formatDuration(60_000)).toBe('1m 00s');
    expect(formatDuration(64_000)).toBe('1m 04s');
    expect(formatDuration(125_000)).toBe('2m 05s');
  });

  it('rounds to the nearest second', () => {
    expect(formatDuration(1_600)).toBe('2s');
    expect(formatDuration(1_400)).toBe('1s');
  });

  it('clamps negative input to 0s', () => {
    expect(formatDuration(-500)).toBe('0s');
  });
});
