import { create } from 'zustand';

/**
 * A one-shot "start a new session" request, raised from anywhere in the nav
 * chrome (the drawer's Sessions "+") and consumed by the Sessions screen, which
 * owns the create sheet. A tiny store rather than a route param so the intent
 * fires exactly once — no `?new=1` left in the URL to re-open the sheet on a
 * later re-render or back-navigation. The drawer requests, then routes to
 * Sessions; that screen opens the sheet on mount (or immediately, if already
 * there) and consumes the flag.
 */
type NewSessionIntent = {
  requested: boolean;
  request: () => void;
  consume: () => void;
};

export const useNewSessionIntent = create<NewSessionIntent>((set) => ({
  requested: false,
  request: () => set({ requested: true }),
  consume: () => set({ requested: false }),
}));
