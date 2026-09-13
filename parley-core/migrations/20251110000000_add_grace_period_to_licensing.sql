-- Migration: Add grace_period column to licensing table
-- This allows per-license grace period configuration instead of using a global constant

-- Add grace_period column (stores seconds of grace period after expiry)
ALTER TABLE licensing ADD COLUMN grace_period INTEGER NOT NULL DEFAULT 604800;

-- Note: Default is 7 days (604800 seconds) for existing licenses
-- New licenses will have grace_period value from their signed payload
