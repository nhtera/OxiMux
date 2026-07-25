import AsyncStorage from '@react-native-async-storage/async-storage';

import { useChatPreferences } from '@/stores/chat-preferences';

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
  useChatPreferences.setState({ toolDetail: 'overview', autoExpandThinking: false, loaded: false });
});

describe('useChatPreferences', () => {
  it('defaults to an overview, thinking collapsed', () => {
    const state = useChatPreferences.getState();
    expect(state.toolDetail).toBe('overview');
    expect(state.autoExpandThinking).toBe(false);
  });

  it('applies and persists the tool-detail choice', () => {
    useChatPreferences.getState().setToolDetail('detailed');
    expect(useChatPreferences.getState().toolDetail).toBe('detailed');
    expect(setItem).toHaveBeenCalledWith('@oximux:tool-detail', 'detailed');
  });

  it('persists the thinking toggle as a flag string', () => {
    useChatPreferences.getState().setAutoExpandThinking(true);
    expect(useChatPreferences.getState().autoExpandThinking).toBe(true);
    expect(setItem).toHaveBeenCalledWith('@oximux:auto-expand-thinking', '1');
  });

  it('restores stored values on load', async () => {
    getItem.mockImplementation((key: string) =>
      Promise.resolve(key === '@oximux:tool-detail' ? 'detailed' : '1')
    );
    await useChatPreferences.getState().load();
    const state = useChatPreferences.getState();
    expect(state.toolDetail).toBe('detailed');
    expect(state.autoExpandThinking).toBe(true);
    expect(state.loaded).toBe(true);
  });

  it('falls back to overview for an unrecognised stored detail', async () => {
    getItem.mockImplementation((key: string) =>
      Promise.resolve(key === '@oximux:tool-detail' ? 'verbose' : '0')
    );
    await useChatPreferences.getState().load();
    expect(useChatPreferences.getState().toolDetail).toBe('overview');
  });

  it('still finishes loading when storage throws', async () => {
    getItem.mockRejectedValue(new Error('storage unavailable'));
    await useChatPreferences.getState().load();
    expect(useChatPreferences.getState().loaded).toBe(true);
  });

  it('does not reject when a write fails', async () => {
    setItem.mockRejectedValue(new Error('disk full'));
    expect(() => useChatPreferences.getState().setToolDetail('detailed')).not.toThrow();
    expect(useChatPreferences.getState().toolDetail).toBe('detailed');
  });
});
