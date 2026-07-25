import type { Recurrence, Schedule, ScheduleRun } from 'oximux-core';
import { useCallback, useEffect, useState } from 'react';

import { useClient } from './client';
import { describeError } from './errors';

/** What the create form collects; the id and first fire are the desktop's to derive. */
export type ScheduleDraft = {
  name: string;
  cwd: string;
  prompt: string;
  recurrence: Recurrence;
};

/**
 * The desktop's schedules, and the writes that manage them.
 *
 * **Not live.** Schedules change on human timescales and firing them is the
 * desktop's job, not the phone's — so there is nothing to stream. The list is
 * fetched on mount and on pull-to-refresh; a write updates local state directly
 * from the row the host returns rather than re-listing.
 *
 * `error` is reserved for a failed round trip — a real, actionable failure the
 * user can retry. An empty list is not an error: a desktop with no schedules is
 * the ordinary case, and the screen says so.
 *
 * A session-scoped or read-only device is refused writes by the host; those
 * refusals surface as `error` on the attempted action, not as a silent no-op.
 */
export function useSchedules() {
  const client = useClient((s) => s.client);
  const [schedules, setSchedules] = useState<Schedule[]>([]);
  // Starts true: the screen mounts already fetching, and a flash of "no
  // schedules" before the first result would say something false.
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();

  const load = useCallback(
    (alive: () => boolean) => {
      if (!client) return Promise.resolve();
      return client
        .listSchedules()
        .then((rows) => {
          if (!alive()) return;
          setSchedules(rows);
          setError(undefined);
        })
        .catch((e: unknown) => {
          if (!alive()) return;
          setError(describeError(e));
        })
        .finally(() => {
          if (alive()) setLoading(false);
        });
    },
    [client],
  );

  useEffect(() => {
    let alive = true;
    void load(() => alive);
    return () => {
      alive = false;
    };
  }, [load]);

  const refresh = useCallback(() => {
    setLoading(true);
    let alive = true;
    return load(() => alive).finally(() => {
      alive = false;
    });
  }, [load]);

  /**
   * Create a schedule, prepending the stored row the host returns. Throws on
   * failure so the form can show the reason and keep the user's input rather
   * than closing over a lost draft.
   */
  const create = useCallback(
    async (draft: ScheduleDraft): Promise<Schedule> => {
      if (!client) throw new Error('Not connected to a desktop.');
      const created = await client.createSchedule(
        draft.name,
        draft.cwd,
        draft.prompt,
        undefined,
        draft.recurrence,
      );
      setSchedules((prev) => [created, ...prev]);
      return created;
    },
    [client],
  );

  /**
   * Toggle a schedule enabled/disabled, updating the row in place. On failure
   * the row is left as it was and the error is surfaced — no optimistic flip
   * that a refused write would leave lying.
   */
  const toggle = useCallback(
    async (id: string, enabled: boolean) => {
      if (!client) return;
      try {
        await client.setScheduleEnabled(id, enabled);
        setSchedules((prev) => prev.map((s) => (s.id === id ? { ...s, enabled } : s)));
        setError(undefined);
      } catch (e) {
        setError(describeError(e));
      }
    },
    [client],
  );

  /** Remove a schedule, dropping it from the list only once the host confirms. */
  const remove = useCallback(
    async (id: string) => {
      if (!client) return;
      try {
        await client.deleteSchedule(id);
        setSchedules((prev) => prev.filter((s) => s.id !== id));
        setError(undefined);
      } catch (e) {
        setError(describeError(e));
      }
    },
    [client],
  );

  return { schedules, loading, error, refresh, create, toggle, remove };
}

/**
 * One schedule's recent run history. Fetched on demand — a fresh schedule has
 * never fired, so an empty result is the norm, not a failure.
 */
export function useScheduleRuns(scheduleId: string, limit = 30) {
  const client = useClient((s) => s.client);
  const [runs, setRuns] = useState<ScheduleRun[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();

  const load = useCallback(
    (alive: () => boolean) => {
      if (!client) return Promise.resolve();
      return client
        .getScheduleRuns(scheduleId, limit)
        .then((rows) => {
          if (!alive()) return;
          setRuns(rows);
          setError(undefined);
        })
        .catch((e: unknown) => {
          if (!alive()) return;
          setError(describeError(e));
        })
        .finally(() => {
          if (alive()) setLoading(false);
        });
    },
    [client, scheduleId, limit],
  );

  useEffect(() => {
    let alive = true;
    void load(() => alive);
    return () => {
      alive = false;
    };
  }, [load]);

  const refresh = useCallback(() => {
    setLoading(true);
    let alive = true;
    return load(() => alive).finally(() => {
      alive = false;
    });
  }, [load]);

  return { runs, loading, error, refresh };
}
