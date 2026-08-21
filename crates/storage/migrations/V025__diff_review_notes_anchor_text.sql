-- V025: remember the code each review note was written against.
--
-- V010 anchored a note to (repo, diff_ref, path, side, line). A line number
-- is a position, not an identity: edit anything above a noted line and the
-- number addresses different code, so the note re-attaches to a line its
-- author never saw — and the markdown prompt built from these rows quotes
-- that wrong code to an agent as the reviewer's subject.
--
-- Storing the line's text turns the ambiguity into three answerable cases on
-- load: same text at that number (still anchored), text found elsewhere in
-- the file (moved — re-anchor), text gone (the note outlived its line, and
-- says so instead of pointing somewhere false).
--
-- Additive, DEFAULT '' — existing rows keep their line and read as
-- unverifiable, which leaves them exactly where they are today rather than
-- detaching a review someone is in the middle of.

ALTER TABLE diff_review_notes ADD COLUMN anchor_text TEXT NOT NULL DEFAULT '';
