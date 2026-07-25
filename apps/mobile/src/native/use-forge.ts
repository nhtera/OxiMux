import { ForgeItemKind, ForgeState, type CheckRun, type ForgeItem } from 'oximux-core';
import { useCallback, useEffect, useState } from 'react';

import { useClient } from './client';
import { describeError } from './errors';

/**
 * A session repository's issues, pull requests and CI checks.
 *
 * **An empty result is not an error state and this hook must not present it as
 * one.** The desktop cannot distinguish "no matching items" from "no forge CLI
 * installed", "CLI signed out", "repo hosted elsewhere" or "network down" — so
 * neither can this. `error` is reserved for a failed *round trip* to the
 * desktop, which is a real, actionable failure; everything else lands as an
 * empty list and the screen says "nothing here".
 *
 * Fetched on demand rather than kept live. Each call shells out to `gh`/`glab`
 * on the desktop and is network-bound, so polling would put a slow, rate-limited
 * request on a timer for data that changes on human timescales. `refresh` is the
 * pull-to-refresh path.
 */
export function useForge(sessionId: string, kind: ForgeItemKind) {
  const client = useClient((s) => s.client);
  const [items, setItems] = useState<ForgeItem[]>([]);
  const [checks, setChecks] = useState<CheckRun[]>([]);
  // Starts true: the screen mounts already fetching, and a flash of "no issues"
  // before the first result lands would say something false.
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();

  /**
   * One round trip, applied only if `alive` still holds.
   *
   * Takes the guard as a callback rather than reading a ref so the effect below
   * can pass its own local flag: these calls are network-bound and can take
   * seconds, which is long enough for the user to have left the screen.
   */
  const load = useCallback(
    (alive: () => boolean) => {
      if (!client) return Promise.resolve();
      return Promise.all([
        client.listForgeItems(sessionId, kind, ForgeState.Open, false),
        client.listForgeChecks(sessionId),
      ])
        .then(([nextItems, nextChecks]) => {
          if (!alive()) return;
          setItems(nextItems);
          setChecks(nextChecks);
          setError(undefined);
        })
        .catch((e: unknown) => {
          // A failed round trip to the desktop — distinct from an empty answer,
          // and worth showing, because the user can act on it (reconnect).
          if (!alive()) return;
          setError(describeError(e));
        })
        .finally(() => {
          if (alive()) setLoading(false);
        });
    },
    [client, sessionId, kind]
  );

  // Written as a promise chain rather than an awaited async call so no setState
  // runs synchronously inside the effect body (the cascading-render rule).
  useEffect(() => {
    let live = true;
    void load(() => live);
    return () => {
      live = false;
    };
  }, [load]);

  const refresh = useCallback(() => {
    setLoading(true);
    return load(() => true);
  }, [load]);

  /**
   * Body + author for one item, fetched when it is opened.
   *
   * Separate from the listing because the body is markdown that can run to
   * kilobytes — pulling it for all 50 rows would make the list far more
   * expensive than what is actually rendered. `null` means the desktop could
   * not supply it, which the detail view shows as "no description available"
   * rather than a blank body.
   */
  const detail = useCallback(
    // Takes the item's `number` verbatim (a `bigint` — the wire field is a
    // u64) rather than converting at this boundary, so there is no place for a
    // lossy narrowing to creep in.
    async (number: bigint): Promise<string | null> => {
      if (!client) return null;
      try {
        const got = await client.forgeItemDetail(sessionId, kind, number);
        return got?.body ?? null;
      } catch {
        return null;
      }
    },
    [client, sessionId, kind]
  );

  return { items, checks, loading, error, refresh, detail };
}
