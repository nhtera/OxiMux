import AsyncStorage from '@react-native-async-storage/async-storage';
import { create } from 'zustand';

/** What the user chose in Settings. `system` defers to the OS. */
export type ThemePreference = 'system' | 'light' | 'dark';

const STORAGE_KEY = '@oximux:theme-preference';

type State = {
  preference: ThemePreference;
  /**
   * False until the stored value has been read.
   *
   * Exposed rather than kept private because the first paint would otherwise
   * flash the OS theme before a stored override loaded — the caller waits on
   * this to avoid showing a light screen to someone who chose dark.
   */
  loaded: boolean;
  setPreference: (preference: ThemePreference) => void;
  load: () => Promise<void>;
};

function isPreference(value: string | null): value is ThemePreference {
  return value === 'system' || value === 'light' || value === 'dark';
}

export const useThemePreference = create<State>((set) => ({
  preference: 'system',
  loaded: false,

  setPreference: (preference) => {
    // Applied immediately; persistence follows. The toggle must feel instant,
    // and a failed write costs the preference on next launch rather than making
    // the tap appear to do nothing now.
    set({ preference });
    void AsyncStorage.setItem(STORAGE_KEY, preference).catch(() => {});
  },

  load: async () => {
    try {
      const stored = await AsyncStorage.getItem(STORAGE_KEY);
      // An unrecognised value (downgrade, corrupt write) falls back to `system`
      // rather than propagating a bad string into the theme lookup.
      set({ preference: isPreference(stored) ? stored : 'system', loaded: true });
    } catch {
      set({ loaded: true });
    }
  },
}));
