/**
 * The amber used by every card that blocks a turn — permission prompts and
 * questions alike. Shared so the two read as one class of "the agent is waiting
 * on you" rather than as two unrelated widgets that happen to look similar.
 *
 * Deliberately a fixed colour rather than a theme token: it has to carry the
 * same "stop and read this" weight in both light and dark, which a token that
 * flips with the scheme would not.
 */
export const QUESTION_ACCENT = '#F5A623';
