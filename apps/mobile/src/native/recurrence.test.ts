import { INTERVAL_CHOICES, MIN_INTERVAL_MINUTES, WEEKDAYS, intervalLabel } from './recurrence';

describe('intervalLabel', () => {
  it('reads sub-hour intervals in minutes', () => {
    expect(intervalLabel(5)).toBe('5 min');
    expect(intervalLabel(30)).toBe('30 min');
  });

  it('collapses whole hours to hours', () => {
    expect(intervalLabel(60)).toBe('1 hour');
    expect(intervalLabel(120)).toBe('2 hours');
    expect(intervalLabel(480)).toBe('8 hours');
  });

  it('keeps a non-whole-hour multiple in minutes', () => {
    // 90 minutes is an hour and a half, not "1 hour" — it stays in minutes rather
    // than rounding to a wrong hour count.
    expect(intervalLabel(90)).toBe('90 min');
  });
});

describe('picker constants', () => {
  it('offers only presets at or above the desktop floor', () => {
    for (const minutes of INTERVAL_CHOICES) {
      expect(minutes).toBeGreaterThanOrEqual(MIN_INTERVAL_MINUTES);
    }
  });

  it('names seven weekdays starting on Monday', () => {
    expect(WEEKDAYS).toHaveLength(7);
    expect(WEEKDAYS[0]).toBe('Mon');
    expect(WEEKDAYS[6]).toBe('Sun');
  });
});
