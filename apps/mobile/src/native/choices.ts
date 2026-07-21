import type { SessionChoices } from 'oximux-core';
import { useCallback, useEffect, useState } from 'react';

import { useClient } from './client';
import { describeError } from './errors';

const EMPTY: SessionChoices = {
  models: [],
  modes: [],
  currentModel: undefined,
  currentMode: undefined,
};

/**
 * The session's model/mode catalog, and the calls that switch between them.
 *
 * Fetched once when the session opens rather than kept live: a backend's
 * catalog does not change during a session, so re-polling it would cost round
 * trips for an answer that never moves. `refresh` exists for the one case that
 * does change it — a dynamic-catalog backend whose handshake completes after
 * the screen mounted, leaving the first fetch legitimately empty.
 */
export function useChoices(sessionId: string) {
  const client = useClient((s) => s.client);
  const [choices, setChoices] = useState<SessionChoices>(EMPTY);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  const refresh = useCallback(async () => {
    if (!client) return;
    try {
      setChoices(await client.listChoices(sessionId));
    } catch {
      // Deliberately silent. A catalog we could not fetch means the pickers stay
      // hidden, which is the same outcome as a backend that offers nothing —
      // and an error row for a control the user never asked for would be noise.
      setChoices(EMPTY);
    }
  }, [client, sessionId]);

  // Written inline rather than as `void refresh()` for two reasons: it keeps
  // setState out of the effect body (the cascading-render rule), and the `alive`
  // guard stops a slow catalog fetch from setting state on a screen the user has
  // already navigated away from.
  useEffect(() => {
    if (!client) return;
    let alive = true;
    client
      .listChoices(sessionId)
      .then((next) => {
        if (alive) setChoices(next);
      })
      .catch(() => {
        // Silent by design — see `refresh`.
        if (alive) setChoices(EMPTY);
      });
    return () => {
      alive = false;
    };
  }, [client, sessionId]);

  /**
   * Switching is reported loudly on failure, unlike fetching: the user tapped
   * something and is owed an answer. A fix-at-spawn backend refuses here, and
   * the host's message says so.
   */
  const pick = useCallback(
    async (kind: 'model' | 'mode', id: string) => {
      if (!client || busy) return;
      setBusy(true);
      setError(undefined);
      try {
        if (kind === 'model') await client.setModel(sessionId, id);
        else await client.setPermissionMode(sessionId, id);
        // Re-read rather than assuming the switch took: the host is the only
        // authority on what is actually running, and optimistically marking the
        // new entry would leave the picker lying if the backend refused.
        await refresh();
      } catch (e) {
        setError(describeError(e));
      } finally {
        setBusy(false);
      }
    },
    [client, sessionId, busy, refresh]
  );

  return {
    choices,
    busy,
    error,
    pick,
    refresh,
    dismissError: () => setError(undefined),
  };
}
