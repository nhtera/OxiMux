import * as Clipboard from 'expo-clipboard';

import { tick } from '@/native/haptics';

/**
 * Copy text to the system clipboard, with a light haptic to confirm the tap
 * registered. Resolves to whether the write landed so a caller can show a
 * transient "Copied" state only when it actually copied.
 *
 * The haptic fires optimistically before the async write: the confirmation is
 * for the gesture, not the result, and `setStringAsync` is effectively instant.
 */
export async function copyText(text: string): Promise<boolean> {
  tick();
  try {
    return await Clipboard.setStringAsync(text);
  } catch {
    return false;
  }
}
