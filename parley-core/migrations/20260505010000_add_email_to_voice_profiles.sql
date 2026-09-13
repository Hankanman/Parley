-- Add an optional email per stored voice profile so a named speaker can be
-- linked to a contact identity (used in summaries / action-item attribution).
-- Email is nullable: not every speaker we name will have one.
ALTER TABLE voice_profiles ADD COLUMN email TEXT;
