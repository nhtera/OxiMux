import AsyncStorage from '@react-native-async-storage/async-storage';
import { create } from 'zustand';

/**
 * How much of each tool call to show by default.
 * - `overview` — collapsed to name + status; tap to expand the result.
 * - `detailed` — the result is shown expanded from the start.
 */
export type ToolDetail = 'overview' | 'detailed';

const DETAIL_KEY = '@oximux:tool-detail';
const THINKING_KEY = '@oximux:auto-expand-thinking';

type State = {
  toolDetail: ToolDetail;
  /** Whether an assistant message's thinking block starts expanded. */
  autoExpandThinking: boolean;
  /** False until the stored values have been read. */
  loaded: boolean;
  setToolDetail: (value: ToolDetail) => void;
  setAutoExpandThinking: (value: boolean) => void;
  load: () => Promise<void>;
};

function isDetail(value: string | null): value is ToolDetail {
  return value === 'overview' || value === 'detailed';
}

export const useChatPreferences = create<State>((set) => ({
  // Overview by default: a long tool result should not dominate the transcript
  // until the user asks to see it.
  toolDetail: 'overview',
  autoExpandThinking: false,
  loaded: false,

  setToolDetail: (value) => {
    set({ toolDetail: value });
    void AsyncStorage.setItem(DETAIL_KEY, value).catch(() => {});
  },

  setAutoExpandThinking: (value) => {
    set({ autoExpandThinking: value });
    void AsyncStorage.setItem(THINKING_KEY, value ? '1' : '0').catch(() => {});
  },

  load: async () => {
    try {
      const [detail, thinking] = await Promise.all([
        AsyncStorage.getItem(DETAIL_KEY),
        AsyncStorage.getItem(THINKING_KEY),
      ]);
      set({
        toolDetail: isDetail(detail) ? detail : 'overview',
        autoExpandThinking: thinking === '1',
        loaded: true,
      });
    } catch {
      set({ loaded: true });
    }
  },
}));
