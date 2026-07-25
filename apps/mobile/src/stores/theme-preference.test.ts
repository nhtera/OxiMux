import AsyncStorage from '@react-native-async-storage/async-storage';

import { useThemePreference } from '@/stores/theme-preference';

jest.mock('@react-native-async-storage/async-storage', () => ({
  getItem: jest.fn(),
  setItem: jest.fn(() => Promise.resolve()),
}));

const getItem = AsyncStorage.getItem as jest.Mock;
const setItem = AsyncStorage.setItem as jest.Mock;

beforeEach(() => {
  getItem.mockReset();
  setItem.mockReset();
  setItem.mockResolvedValue(undefined);
  useThemePreference.setState({ preference: 'system', loaded: false });
});

describe('useThemePreference', () => {
  it('defaults to following the OS', () => {
    expect(useThemePreference.getState().preference).toBe('system');
  });

  it('applies a choice immediately and persists it', () => {
    useThemePreference.getState().setPreference('dark');
    // Applied synchronously: the toggle must feel instant, so the write is not
    // awaited before the UI reflects the change.
    expect(useThemePreference.getState().preference).toBe('dark');
    expect(setItem).toHaveBeenCalledWith('@oximux:theme-preference', 'dark');
  });

  it('restores a stored choice on load', async () => {
    getItem.mockResolvedValue('light');
    await useThemePreference.getState().load();
    expect(useThemePreference.getState().preference).toBe('light');
    expect(useThemePreference.getState().loaded).toBe(true);
  });

  it('falls back to system for an unrecognised stored value', async () => {
    // A downgrade or a corrupt write must not push a bad string into the theme
    // lookup, where it would resolve to no colours at all.
    getItem.mockResolvedValue('chartreuse');
    await useThemePreference.getState().load();
    expect(useThemePreference.getState().preference).toBe('system');
  });

  it('still finishes loading when storage throws', async () => {
    getItem.mockRejectedValue(new Error('storage unavailable'));
    await useThemePreference.getState().load();
    // `loaded` must flip regardless, or a caller gating first paint on it would
    // wait forever.
    expect(useThemePreference.getState().loaded).toBe(true);
    expect(useThemePreference.getState().preference).toBe('system');
  });

  it('does not reject when the write fails', async () => {
    setItem.mockRejectedValue(new Error('disk full'));
    expect(() => useThemePreference.getState().setPreference('dark')).not.toThrow();
    expect(useThemePreference.getState().preference).toBe('dark');
  });
});
