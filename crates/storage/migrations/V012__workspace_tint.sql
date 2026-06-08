-- Optional per-workspace identifier hue (a tab-color swatch slug, e.g. "blue").
-- Drawn from the existing 9-swatch palette; nullable = no tint (pure charcoal).
ALTER TABLE workspaces ADD COLUMN tint TEXT;
