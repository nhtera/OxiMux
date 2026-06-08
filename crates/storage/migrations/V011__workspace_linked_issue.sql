-- Link a workspace back to the GitHub issue/PR it was created from, so the
-- workspace card can show a "#<n>" badge that survives restart. Nullable:
-- manually-created workspaces have no linked issue.
ALTER TABLE workspaces ADD COLUMN linked_issue TEXT;
